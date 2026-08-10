use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use atomic::Ordering;
use calloop::ping::{make_ping, Ping};
use calloop::timer::{TimeoutAction, Timer};
use calloop::LoopHandle;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::compositor::{Blocker, BlockerState};

/// Default time limit, after which the transaction completes.
///
/// Serves to avoid hanging when a client fails to respond to a configure promptly.
const TIME_LIMIT: Duration = Duration::from_millis(300);

/// Arms a transaction's deadline on the event loop.
type DeadlineRegistrar = Rc<dyn Fn(&Transaction)>;

// Installed once, at startup, by whoever owns the loop; absent in tests, which drive
// completion themselves.
thread_local! {
    static DEADLINE_REGISTRAR: RefCell<Option<DeadlineRegistrar>> = const { RefCell::new(None) };
}

/// Teach transactions how to arm their own deadline.
///
/// Call once with a handle to the loop everything runs on.
pub fn set_deadline_registrar<T: 'static>(event_loop: LoopHandle<'static, T>) {
    let register: DeadlineRegistrar =
        Rc::new(move |transaction: &Transaction| transaction.register_deadline_timer(&event_loop));
    DEADLINE_REGISTRAR.with_borrow_mut(|slot| *slot = Some(register));
}

/// Transaction between Wayland clients.
///
/// How to use it:
/// 1. Create a transaction with [`Transaction::new()`].
/// 2. Clone it as many times as you need.
/// 3. Before adding the transaction as a commit blocker, remember to call
///    [`Transaction::add_notification()`] to receive a notification when the transaction completes.
/// 4. In your surface pre-commit handler, if the transaction corresponding to that commit isn't
///    ready, get a blocker with [`Transaction::blocker()`] and add it to the surface.
///
/// A transaction always completes. It completes early when its last clone is dropped, and at
/// its deadline otherwise — the deadline is armed by the first clone, which is the moment it
/// stops being solely its creator's to complete. This used to be step 4 of the list above,
/// something every creator had to remember, and the one that never did was the layout: it has
/// no loop handle and no business holding one. A client that was sent a configure and never
/// committed then held its clone for good, and the workspace it was in stopped laying out —
/// not just for that window, for everything in it, until something else rebuilt the tree.
#[derive(Debug)]
pub struct Transaction {
    inner: Arc<Inner>,
    deadline: Rc<RefCell<Deadline>>,
}

impl Clone for Transaction {
    fn clone(&self) -> Self {
        // Handing out a clone is what makes someone else's silence able to hold this open, so
        // it is also what the deadline is for. Arming it here rather than in `new` costs
        // nothing for the transactions that never leave home.
        if matches!(&*self.deadline.borrow(), Deadline::NotRegistered(_)) {
            let registrar = DEADLINE_REGISTRAR.with_borrow(|slot| slot.clone());
            if let Some(register) = registrar {
                register(self);
            }
        }

        Self {
            inner: Arc::clone(&self.inner),
            deadline: Rc::clone(&self.deadline),
        }
    }
}

/// Blocker for a [`Transaction`].
#[derive(Debug)]
pub struct TransactionBlocker(Weak<Inner>);

#[derive(Debug)]
enum Deadline {
    NotRegistered(Instant),
    Registered {
        remove: Ping,
    },
    /// Armed by a stand-in registrar; only tests produce this.
    #[cfg(test)]
    Armed,
}

#[derive(Debug)]
struct Inner {
    /// Whether the transaction is completed.
    completed: AtomicBool,
    /// Notifications to send out upon completing the transaction.
    notifications: Mutex<Option<(Sender<Client>, Vec<Client>)>>,
}

impl Transaction {
    /// Creates a new transaction.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
            deadline: Rc::new(RefCell::new(Deadline::NotRegistered(
                Instant::now() + TIME_LIMIT,
            ))),
        }
    }

    /// Gets a blocker for this transaction.
    pub fn blocker(&self) -> TransactionBlocker {
        trace!(transaction = ?Arc::as_ptr(&self.inner), "generating blocker");
        TransactionBlocker(Arc::downgrade(&self.inner))
    }

    /// Adds a notification for when this transaction completes.
    pub fn add_notification(&self, sender: Sender<Client>, client: Client) {
        if self.is_completed() {
            error!("tried to add notification to a completed transaction");
            return;
        }

        let mut guard = self.inner.notifications.lock().unwrap();
        guard.get_or_insert((sender, Vec::new())).1.push(client);
    }

    /// Registers this transaction's deadline timer on an event loop.
    fn register_deadline_timer<T: 'static>(&self, event_loop: &LoopHandle<'static, T>) {
        let mut cell = self.deadline.borrow_mut();
        if let Deadline::NotRegistered(deadline) = *cell {
            let timer = Timer::from_deadline(deadline);
            let inner = Arc::downgrade(&self.inner);
            let token = event_loop
                .insert_source(timer, move |_, _, _| {
                    let _span = trace_span!("deadline timer", transaction = ?Weak::as_ptr(&inner))
                        .entered();

                    // FIXME: come up with some way to control the deadline timer from tests.
                    #[cfg(not(test))]
                    if let Some(inner) = inner.upgrade() {
                        trace!("deadline reached, completing transaction");
                        inner.complete();
                    } else {
                        // We should remove the timer automatically. But this callback can still
                        // just happen to run while the ping callback is scheduled, leading to this
                        // branch being legitimately taken.
                        trace!("transaction completed without removing the timer");
                    }

                    TimeoutAction::Drop
                })
                .unwrap();

            // Add a ping source that will be used to remove the timer automatically.
            let (ping, source) = make_ping().unwrap();
            let loop_handle = event_loop.clone();
            event_loop
                .insert_source(source, move |_, _, _| {
                    loop_handle.remove(token);
                })
                .unwrap();

            *cell = Deadline::Registered { remove: ping };
        }
    }

    /// Returns whether this transaction has already completed.
    pub fn is_completed(&self) -> bool {
        self.inner.is_completed()
    }

    /// Returns whether this is the last instance of this transaction.
    pub fn is_last(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _span = trace_span!("drop", transaction = ?Arc::as_ptr(&self.inner)).entered();

        if self.is_last() {
            // If this was the last transaction, complete it.
            trace!("last transaction dropped, completing");
            self.inner.complete();

            // Also remove the timer.
            if let Deadline::Registered { remove } = &*self.deadline.borrow() {
                remove.ping();
            };
        }
    }
}

impl TransactionBlocker {
    pub fn completed() -> Self {
        Self(Weak::new())
    }
}

impl Blocker for TransactionBlocker {
    fn state(&self) -> BlockerState {
        if self.0.upgrade().is_none_or(|x| x.is_completed()) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

impl Inner {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            notifications: Mutex::new(None),
        }
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Relaxed)
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Relaxed);

        let mut guard = self.notifications.lock().unwrap();
        if let Some((sender, clients)) = guard.take() {
            for client in clients {
                if let Err(err) = sender.send(client) {
                    warn!("error sending blocker notification: {err:?}");
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transaction that has been handed to someone else must arm its deadline, because from
    /// that moment its completion is not solely its creator's to decide. Every creator used to
    /// be asked to remember, and the layout — which has no loop handle — never did.
    #[test]
    fn handing_out_a_clone_arms_the_deadline() {
        let armed = Rc::new(RefCell::new(0usize));
        let counter = Rc::clone(&armed);
        DEADLINE_REGISTRAR.with_borrow_mut(|slot| {
            *slot = Some(Rc::new(move |transaction: &Transaction| {
                *counter.borrow_mut() += 1;
                // Stand in for the loop: mark it registered so it is not armed twice.
                *transaction.deadline.borrow_mut() = Deadline::Armed;
            }));
        });

        let transaction = Transaction::new();
        assert_eq!(
            *armed.borrow(),
            0,
            "a transaction nobody holds needs no timer"
        );

        let first = transaction.clone();
        assert_eq!(*armed.borrow(), 1, "the first clone arms it");

        let _second = transaction.clone();
        let _third = first.clone();
        assert_eq!(*armed.borrow(), 1, "and later ones do not arm it again");

        DEADLINE_REGISTRAR.with_borrow_mut(|slot| *slot = None);
    }
}
