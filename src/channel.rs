#![allow(unused_imports)]

use {io, Evented, EventSet, Poll, PollOpt, Registration, SetReadiness, Token};
use lazy::{Lazy, AtomicLazy};
use std::sync::{mpsc, Arc};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
pub struct SenderCtl {
    inner: Arc<Inner>,
}

pub struct ReceiverCtl {
    registration: Lazy<Registration>,
    inner: Arc<Inner>,
}

pub struct Sender<T> {
    tx: StdSender<T>,
    ctl: SenderCtl,
}

enum StdSender<T> {
    Bounded(mpsc::SyncSender<T>),
    Unbounded(mpsc::Sender<T>),
}

pub struct Receiver<T> {
    rx: mpsc::Receiver<T>,
    ctl: ReceiverCtl,
}

#[derive(Debug)]
pub enum SendError<T> {
    Io(io::Error),
    Disconnected(T),
}

#[derive(Debug)]
pub enum TrySendError<T> {
    Io(io::Error),
    Full(T),
    Disconnected(T),
}

struct Inner {
    pending: AtomicUsize,
    set_readiness: AtomicLazy<SetReadiness>,
}

pub fn from_std_channel<T>((tx, rx): (mpsc::Sender<T>, mpsc::Receiver<T>)) -> (Sender<T>, Receiver<T>)
{
    let (tx_ctl, rx_ctl) = ctl_pair();

    let tx = Sender {
        tx: StdSender::Unbounded(tx),
        ctl: tx_ctl,
    };

    let rx = Receiver {
        rx: rx,
        ctl: rx_ctl,
    };

    (tx, rx)
}

pub fn from_std_sync_channel<T>((tx, rx): (mpsc::SyncSender<T>, mpsc::Receiver<T>)) -> (Sender<T>, Receiver<T>)
{
    let (tx_ctl, rx_ctl) = ctl_pair();

    let tx = Sender {
        tx: StdSender::Bounded(tx),
        ctl: tx_ctl,
    };

    let rx = Receiver {
        rx: rx,
        ctl: rx_ctl,
    };

    (tx, rx)
}

pub fn ctl_pair() -> (SenderCtl, ReceiverCtl) {
    let inner = Arc::new(Inner {
        pending: AtomicUsize::new(0),
        set_readiness: AtomicLazy::new(),
    });

    let tx = SenderCtl {
        inner: inner.clone(),
    };

    let rx = ReceiverCtl {
        registration: Lazy::new(),
        inner: inner,
    };

    (tx, rx)
}

impl SenderCtl {
    /// Call to track that a message has been sent
    pub fn inc(&self) -> io::Result<()> {
        if 0 == self.inner.pending.fetch_add(1, Ordering::Acquire) {
            // Toggle readiness to readable
            if let Some(set_readiness) = self.inner.set_readiness.as_ref() {
                try!(set_readiness.set_readiness(EventSet::readable()));
            }
        }

        Ok(())
    }
}

impl ReceiverCtl {
    pub fn dec(&self) -> io::Result<()> {
        let first = self.inner.pending.load(Ordering::Acquire);

        if first == 1 {
            // Unset readiness
            if let Some(set_readiness) = self.inner.set_readiness.as_ref() {
                try!(set_readiness.set_readiness(EventSet::none()));
            }
        }

        // Decrement
        let second = self.inner.pending.fetch_sub(1, Ordering::AcqRel);

        if first == 1 && second > 0 {
            // There are still pending messages. Since readiness was
            // previously unset, it must be reset here
            if let Some(set_readiness) = self.inner.set_readiness.as_ref() {
                try!(set_readiness.set_readiness(EventSet::none()));
            }
        }

        Ok(())
    }
}

impl Evented for ReceiverCtl {
    fn register(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        if self.registration.is_some() {
            return Err(io::Error::new(io::ErrorKind::Other, "receiver already registered"));
        }

        let (registration, set_readiness) = Registration::new(poll, token, interest, opts);


        if self.inner.pending.load(Ordering::Relaxed) > 0 {
            // TODO: Don't drop readiness
            let _ = set_readiness.set_readiness(EventSet::readable());
        }

        self.registration.set(registration).ok().expect("unexpected state encountered");
        self.inner.set_readiness.set(set_readiness).ok().expect("unexpected state encountered");

        Ok(())
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        match self.registration.as_ref() {
            Some(registration) => registration.update(poll, token, interest, opts),
            None => Err(io::Error::new(io::ErrorKind::Other, "receiver not registered")),
        }
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        match self.registration.as_ref() {
            Some(registration) => registration.deregister(poll),
            None => Err(io::Error::new(io::ErrorKind::Other, "receiver not registered")),
        }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        self.tx.send(t).and_then(|_| {
            try!(self.ctl.inc());
            Ok(())
        })
    }

    pub fn try_send(&self, t: T) -> Result<(), TrySendError<T>> {
        self.tx.try_send(t).and_then(|_| {
            try!(self.ctl.inc());
            Ok(())
        })
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender {
            tx: self.tx.clone(),
            ctl: self.ctl.clone(),
        }
    }
}

impl<T> StdSender<T> {
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        match *self {
            StdSender::Bounded(ref tx) => tx.send(t).map_err(SendError::from),
            StdSender::Unbounded(ref tx) => tx.send(t).map_err(SendError::from),
        }
    }

    pub fn try_send(&self, t: T) -> Result<(), TrySendError<T>> {
        match *self {
            StdSender::Bounded(ref tx) => tx.try_send(t).map_err(TrySendError::from),
            StdSender::Unbounded(ref tx) => tx.send(t).map_err(TrySendError::from),
        }
    }
}

impl<T> Clone for StdSender<T> {
    fn clone(&self) -> StdSender<T> {
        match *self {
            StdSender::Bounded(ref v) => StdSender::Bounded(v.clone()),
            StdSender::Unbounded(ref v) => StdSender::Unbounded(v.clone()),
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, mpsc::TryRecvError> {
        self.rx.try_recv().and_then(|res| {
            let _ = self.ctl.dec();
            Ok(res)
        })
    }
}

impl<T> Evented for Receiver<T> {
    fn register(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        self.ctl.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        self.ctl.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.ctl.deregister(poll)
    }
}

impl<T> From<mpsc::SendError<T>> for SendError<T> {
    fn from(src: mpsc::SendError<T>) -> SendError<T> {
        SendError::Disconnected(src.0)
    }
}

impl<T> From<io::Error> for SendError<T> {
    fn from(src: io::Error) -> SendError<T> {
        SendError::Io(src)
    }
}

impl<T> From<mpsc::TrySendError<T>> for TrySendError<T> {
    fn from(src: mpsc::TrySendError<T>) -> TrySendError<T> {
        match src {
            mpsc::TrySendError::Full(v) => TrySendError::Full(v),
            mpsc::TrySendError::Disconnected(v) => TrySendError::Disconnected(v),
        }
    }
}

impl<T> From<mpsc::SendError<T>> for TrySendError<T> {
    fn from(src: mpsc::SendError<T>) -> TrySendError<T> {
        TrySendError::Disconnected(src.0)
    }
}

impl<T> From<io::Error> for TrySendError<T> {
    fn from(src: io::Error) -> TrySendError<T> {
        TrySendError::Io(src)
    }
}
