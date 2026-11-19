use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

struct State<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
    receiver_open: bool,
}

pub struct Sender<T>(Arc<Mutex<State<T>>>);
pub struct Receiver<T>(Arc<Mutex<State<T>>>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let state = Arc::new(Mutex::new(State {
        queue: VecDeque::new(),
        waker: None,
        senders: 1,
        receiver_open: true,
    }));
    (Sender(state.clone()), Receiver(state))
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), T> {
        let waker = {
            let mut state = self.0.lock().unwrap();
            if !state.receiver_open {
                return Err(value);
            }
            state.queue.push_back(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.0.lock().unwrap();
            state.senders -= 1;
            if state.senders == 0 {
                state.waker.take()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Receiver<T> {
    pub fn recv(&mut self) -> impl Future<Output = Option<T>> + '_ {
        poll_fn(|cx| {
            let mut state = self.0.lock().unwrap();
            if let Some(value) = state.queue.pop_front() {
                Poll::Ready(Some(value))
            } else if state.senders == 0 {
                Poll::Ready(None)
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    pub fn try_recv(&self) -> Option<T> {
        self.0.lock().unwrap().queue.pop_front()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (queue, waker) = {
            let mut state = self.0.lock().unwrap();
            state.receiver_open = false;
            (std::mem::take(&mut state.queue), state.waker.take())
        };
        drop((queue, waker));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake};

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn delivery_order() {
        let (tx, mut rx) = channel();
        assert_eq!(rx.try_recv(), None);
        tx.send(1).unwrap();
        tx.clone().send(2).unwrap();
        drop(tx);
        let waker = Waker::from(Arc::new(Counter::default()));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(pin!(rx.recv()).poll(&mut cx), Poll::Ready(Some(1)));
        assert_eq!(rx.try_recv(), Some(2));
        assert_eq!(pin!(rx.recv()).poll(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn send_wakes_waiting_receiver() {
        let (tx, mut rx) = channel();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut recv = pin!(rx.recv());
        assert_eq!(recv.as_mut().poll(&mut cx), Poll::Pending);
        std::thread::spawn(move || tx.send(7).unwrap())
            .join()
            .unwrap();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(recv.as_mut().poll(&mut cx), Poll::Ready(Some(7)));
    }

    #[test]
    fn last_sender_drop_closes_and_wakes() {
        let (tx, mut rx) = channel::<()>();
        let other = tx.clone();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut recv = pin!(rx.recv());
        assert_eq!(recv.as_mut().poll(&mut cx), Poll::Pending);
        drop(tx);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        assert_eq!(recv.as_mut().poll(&mut cx), Poll::Pending);
        drop(other);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(recv.as_mut().poll(&mut cx), Poll::Ready(None));
    }

    #[test]
    fn send_fails_after_receiver_drop() {
        let (tx, rx) = channel();
        tx.send(1).unwrap();
        drop(rx);
        assert_eq!(tx.send(2), Err(2));
        assert_eq!(tx.clone().send(3), Err(3));
    }
}
