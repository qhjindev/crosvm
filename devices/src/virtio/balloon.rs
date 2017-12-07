// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std;
use std::cmp;
use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use sys_util::{self, EventFd, GuestAddress, GuestMemory, Pollable, Poller};

use super::{VirtioDevice, Queue, DescriptorChain, INTERRUPT_STATUS_CONFIG_CHANGED,
            INTERRUPT_STATUS_USED_RING, TYPE_BALLOON};

#[derive(Debug)]
pub enum BalloonError {
    /// Request to adjust memory size can't provide the number of pages requested.
    NotEnoughPages,
    /// Failure wriitng the config notification event.
    WritingConfigEvent(sys_util::Error),
}
pub type Result<T> = std::result::Result<T, BalloonError>;

// Balloon has three virt IO queues: Inflate, Deflate, and Stats.
// Stats is currently not used.
const QUEUE_SIZE: u16 = 128;
const QUEUE_SIZES: &'static [u16] = &[QUEUE_SIZE, QUEUE_SIZE];

const VIRTIO_BALLOON_PFN_SHIFT: u32 = 12;

// The feature bitmap for virtio balloon
const VIRTIO_BALLOON_F_MUST_TELL_HOST: u32 = 0x01; // Tell before reclaiming pages
const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 0x04; // Deflate balloon on OOM

// BalloonConfig is modified by the worker and read from the device thread.
#[derive(Default)]
struct BalloonConfig {
    num_pages: AtomicUsize,
    actual_pages: AtomicUsize,
}

struct Worker {
    mem: GuestMemory,
    inflate_queue: Queue,
    deflate_queue: Queue,
    interrupt_status: Arc<AtomicUsize>,
    interrupt_evt: EventFd,
    config: Arc<BalloonConfig>,
    command_socket: UnixDatagram,
}

fn valid_inflate_desc(desc: &DescriptorChain) -> bool {
    !desc.is_write_only() && desc.len % 4 == 0
}

impl Worker {
    fn process_inflate_deflate(&mut self, inflate: bool) -> bool {
        let queue = if inflate {
            &mut self.inflate_queue
        } else {
            &mut self.deflate_queue
        };

        let mut used_desc_heads = [0; QUEUE_SIZE as usize];
        let mut used_count = 0;
        for avail_desc in queue.iter(&self.mem) {
            if inflate {
                if valid_inflate_desc(&avail_desc) {
                    let num_addrs = avail_desc.len / 4;
                    'addr_loop: for i in 0..num_addrs as usize {
                        let addr = match avail_desc.addr.checked_add(i * 4) {
                            Some(a) => a,
                            None => break,
                        };
                        let guest_input: u32 = match self.mem.read_obj_from_addr(addr) {
                            Ok(a) => a,
                            Err(_) => continue,
                        };
                        let guest_address =
                            GuestAddress((guest_input as usize) << VIRTIO_BALLOON_PFN_SHIFT);

                        if self.mem
                            .dont_need_range(guest_address, 1 << VIRTIO_BALLOON_PFN_SHIFT)
                            .is_err()
                        {
                            warn!("Marking pages unused failed {:?}", guest_address);
                            continue;
                        }
                    }
                }
            }

            used_desc_heads[used_count] = avail_desc.index;
            used_count += 1;
        }

        for &desc_index in &used_desc_heads[..used_count] {
            queue.add_used(&self.mem, desc_index, 0);
        }
        used_count > 0
    }

    fn signal_used_queue(&self) {
        self.interrupt_status.fetch_or(
            INTERRUPT_STATUS_USED_RING as usize,
            Ordering::SeqCst,
        );
        self.interrupt_evt.write(1).unwrap();
    }

    fn signal_config_changed(&self) {
        self.interrupt_status.fetch_or(
            INTERRUPT_STATUS_CONFIG_CHANGED as
                usize,
            Ordering::SeqCst,
        );
        self.interrupt_evt.write(1).unwrap();
    }

    fn run(&mut self, mut queue_evts: Vec<EventFd>, kill_evt: EventFd) {
        const POLL_INFLATE: u32 = 0;
        const POLL_DEFLATE: u32 = 1;
        const POLL_COMMAND_SOCKET: u32 = 2;
        const POLL_KILL: u32 = 3;

        let inflate_queue_evt = queue_evts.remove(0);
        let deflate_queue_evt = queue_evts.remove(0);

        let mut poller = Poller::new(5);
        'poll: loop {
            let tokens = match poller.poll(
                &[
                    (POLL_INFLATE, &inflate_queue_evt),
                    (POLL_DEFLATE, &deflate_queue_evt),
                    (POLL_COMMAND_SOCKET, &self.command_socket as &Pollable),
                    (POLL_KILL, &kill_evt),
                ],
            ) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed polling for events: {:?}", e);
                    break 'poll;
                }
            };

            let mut needs_interrupt = false;
            'read_tokens: for &token in tokens {
                match token {
                    POLL_INFLATE => {
                        if let Err(e) = inflate_queue_evt.read() {
                            error!("failed reading inflate queue EventFd: {:?}", e);
                            break 'poll;
                        }
                        needs_interrupt |= self.process_inflate_deflate(true);
                    }
                    POLL_DEFLATE => {
                        if let Err(e) = deflate_queue_evt.read() {
                            error!("failed reading deflate queue EventFd: {:?}", e);
                            break 'poll;
                        }
                        needs_interrupt |= self.process_inflate_deflate(false);
                    }
                    POLL_COMMAND_SOCKET => {
                        let mut buf = [0u8; 4];
                        if let Ok(count) = self.command_socket.recv(&mut buf) {
                            if count == 4 {
                                let mut buf = &buf[0..];
                                let increment: i32 = buf.read_i32::<LittleEndian>().unwrap();
                                let num_pages = self.config.num_pages.load(Ordering::Relaxed) as
                                    i32;
                                if increment < 0 && increment.abs() > num_pages {
                                    continue 'read_tokens;
                                }
                                self.config.num_pages.fetch_add(
                                    increment as usize,
                                    Ordering::Relaxed,
                                );
                                self.signal_config_changed();
                            }
                        }
                    }
                    POLL_KILL => break 'poll,
                    _ => unreachable!(),
                }
            }
            if needs_interrupt {
                self.signal_used_queue();
            }
        }
    }
}

/// Virtio device for memory balloon inflation/deflation.
pub struct Balloon {
    command_socket: Option<UnixDatagram>,
    config: Arc<BalloonConfig>,
    features: u32,
    kill_evt: Option<EventFd>,
}

impl Balloon {
    /// Create a new virtio balloon device.
    pub fn new(command_socket: UnixDatagram) -> Result<Balloon> {
        Ok(Balloon {
            command_socket: Some(command_socket),
            config: Arc::new(BalloonConfig {
                num_pages: AtomicUsize::new(0),
                actual_pages: AtomicUsize::new(0),
            }),
            kill_evt: None,
            // TODO(dgreid) - Add stats queue feature.
            features: VIRTIO_BALLOON_F_MUST_TELL_HOST | VIRTIO_BALLOON_F_DEFLATE_ON_OOM,
        })
    }
}

impl Drop for Balloon {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.kill_evt.take() {
            // Ignore the result because there is nothing we can do with a failure.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Balloon {
    fn keep_fds(&self) -> Vec<RawFd> {
        vec![self.command_socket.as_ref().unwrap().as_raw_fd()]
    }

    fn device_type(&self) -> u32 {
        TYPE_BALLOON
    }

    fn queue_max_sizes(&self) -> &[u16] {
        QUEUE_SIZES
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        if offset >= 8 {
            return;
        }
        let num_pages = self.config.num_pages.load(Ordering::Relaxed) as u32;
        let actual_pages = self.config.actual_pages.load(Ordering::Relaxed) as u32;
        let mut config = [0u8; 8];
        // These writes can't fail as they fit in the declared array so unwrap is fine.
        (&mut config[0..])
            .write_u32::<LittleEndian>(num_pages)
            .unwrap();
        (&mut config[4..])
            .write_u32::<LittleEndian>(actual_pages)
            .unwrap();
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against the length of config.
            data.write(&config[offset as usize..cmp::min(end, 8) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, mut data: &[u8]) {
        // Only allow writing to `actual` pages from the guest.
        if offset != 4 || data.len() != 4 {
            return;
        }
        // This read can't fail as it fits in the declared array so unwrap is fine.
        let new_actual: u32 = data.read_u32::<LittleEndian>().unwrap();
        self.config.actual_pages.store(
            new_actual as usize,
            Ordering::Relaxed,
        );
    }

    fn features(&self, page: u32) -> u32 {
        match page {
            0 => VIRTIO_BALLOON_F_MUST_TELL_HOST | VIRTIO_BALLOON_F_DEFLATE_ON_OOM,
            _ => 0u32,
        }
    }

    fn ack_features(&mut self, page: u32, value: u32) {
        match page {
            0 => self.features = self.features & value,
            _ => (),
        };
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt_evt: EventFd,
        status: Arc<AtomicUsize>,
        mut queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) {
        if queues.len() != QUEUE_SIZES.len() || queue_evts.len() != QUEUE_SIZES.len() {
            return;
        }

        let (self_kill_evt, kill_evt) =
            match EventFd::new().and_then(|e| Ok((e.try_clone()?, e))) {
                Ok(v) => v,
                Err(e) => {
                    error!("failed to create kill EventFd pair: {:?}", e);
                    return;
                }
            };
        self.kill_evt = Some(self_kill_evt);

        let config = self.config.clone();
        let command_socket = self.command_socket.take().unwrap();
        let worker_result = thread::Builder::new()
            .name("virtio_balloon".to_string())
            .spawn(move || {
                let mut worker = Worker {
                    mem: mem,
                    inflate_queue: queues.remove(0),
                    deflate_queue: queues.remove(0),
                    interrupt_status: status,
                    interrupt_evt: interrupt_evt,
                    command_socket: command_socket,
                    config: config,
                };
                worker.run(queue_evts, kill_evt);
            });
        if let Err(e) = worker_result {
            error!("failed to spawn virtio_balloon worker: {}", e);
            return;
        }
    }
}