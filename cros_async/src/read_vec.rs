// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::io_source::IoSource;
//TODO - move memvec to uring_mem
use crate::uring_executor::{MemVec, Result};
use crate::uring_fut::UringFutState;
use crate::uring_mem::VecIoWrapper;

/// Future for the `read_to_vectored` function.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadVec<'a, R: IoSource + ?Sized> {
    reader: &'a R,
    state: UringFutState<(u64, Rc<VecIoWrapper>), Rc<VecIoWrapper>>,
}

impl<R: IoSource + ?Sized + Unpin> Unpin for ReadVec<'_, R> {}

impl<'a, R: IoSource + ?Sized + Unpin> ReadVec<'a, R> {
    pub(crate) fn new(reader: &'a R, file_offset: u64, vec: Vec<u8>) -> Self {
        ReadVec {
            reader,
            state: UringFutState::new((file_offset, Rc::new(VecIoWrapper::from(vec)))),
        }
    }
}

impl<R: IoSource + ?Sized + Unpin> Future for ReadVec<'_, R> {
    type Output = Result<Vec<u8>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = std::mem::replace(&mut self.state, UringFutState::Processing);
        let (new_state, ret) = match state.advance(
            |(file_offset, wrapped_vec)| {
                Ok((
                    Pin::new(&self.reader).read_to_mem(
                        file_offset,
                        Rc::<VecIoWrapper>::clone(&wrapped_vec),
                        &[MemVec {
                            offset: 0,
                            len: wrapped_vec.len(),
                        }],
                    )?,
                    wrapped_vec,
                ))
            },
            |op| Pin::new(&self.reader).poll_complete(cx, op),
        ) {
            Ok(d) => d,
            Err(e) => return Poll::Ready(Err(e)),
        };

        self.state = new_state;

        match ret {
            Poll::Pending => Poll::Pending,
            Poll::Ready((r, wrapped_vec)) => match r {
                Ok(_) => Poll::Ready(Ok(Rc::try_unwrap(wrapped_vec)
                    .expect("too many refs on vec")
                    .into())),
                Err(e) => Poll::Ready(Err(e)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::pin_mut;
    use std::fs::File;

    use crate::io_ext::IoSourceExt;

    #[test]
    fn readvec() {
        async fn go() {
            let f = File::open("/dev/zero").unwrap();
            let source = crate::io_source::AsyncSource::new(f).unwrap();
            let v = vec![0x55u8; 32];
            let v_ptr = v.as_ptr();
            let ret_v = source.read_to_vec(0, v).await.unwrap();
            assert_eq!(v_ptr, ret_v.as_ptr());
            assert!(ret_v.iter().all(|&b| b == 0));
        }

        let fut = go();
        pin_mut!(fut);
        crate::run_one(fut).unwrap();
    }
}