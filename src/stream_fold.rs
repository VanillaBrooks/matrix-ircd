// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures3::future::{Future, FutureExt};
use futures3::stream::{Stream, StreamExt};
use futures3::task::{Context, Poll};

use std::boxed::Box;
use std::cell::RefCell;
use std::mem;
use std::ops::Deref;
use std::pin::Pin;

#[must_use = "futures do nothing unless polled"]
pub struct StreamFold<S, W, T, V>
where
    S: Stream<Item = Result<T, V>>,
{
    stream: RefCell<Pin<Box<S>>>,
    // function takes in the accumulator (state) for the first argument and
    // the output from the stream as the second argument
    // returns true if it is finished accumulating and we should complete the future
    // In the future it might just return Poll.
    function: Box<dyn Fn(&RefCell<W>, Result<T, V>) -> bool>,
    // The variable that is being updated with every new value from self.stream
    state: RefCell<W>,
}

impl<W, T, V, S: Stream<Item = Result<T, V>>> StreamFold<S, W, T, V> {
    pub fn new(
        stream: S,
        state: W,
        function: Box<dyn Fn(&RefCell<W>, Result<T, V>) -> bool>,
    ) -> StreamFold<S, W, T, V> {
        StreamFold {
            state: RefCell::new(state),
            stream: RefCell::new(Box::pin(stream)),
            function,
        }
    }

    pub fn into_parts(self) -> (Pin<Box<S>>, W) {
        (self.stream.into_inner(), self.state.into_inner())
    }
}

impl<W, T, V, S: Stream<Item = Result<T, V>>> Future for &StreamFold<S, W, T, V> {
    type Output = Option<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.stream.borrow_mut().as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if (self.function)(&self.state, item) {
                        return Poll::Ready(Some(()));
                    } else {
                        return Poll::Pending;
                    }
                    //
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}
