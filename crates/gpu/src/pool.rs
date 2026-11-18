//! Fixed-size pool of reusable resources handed out as leases.
//!
//! The pool never grows: when every item is leased, `try_acquire` returns
//! `None` and the caller decides what to drop. A lease returns its item to
//! the pool when dropped, from any thread.

use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

pub struct Pool<T> {
    inner: Arc<PoolInner<T>>,
}

struct PoolInner<T> {
    free: Mutex<Vec<T>>,
    capacity: usize,
}

impl<T> Pool<T> {
    /// Creates a pool owning `items`; its capacity is `items.len()`.
    pub fn new(items: Vec<T>) -> Self {
        let capacity = items.len();
        Self {
            inner: Arc::new(PoolInner {
                free: Mutex::new(items),
                capacity,
            }),
        }
    }

    /// Takes a free item, or `None` if all are leased.
    pub fn try_acquire(&self) -> Option<Lease<T>> {
        let item = self.inner.free.lock().expect("pool lock").pop()?;
        Some(Lease {
            item: Some(item),
            pool: Arc::clone(&self.inner),
        })
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn available(&self) -> usize {
        self.inner.free.lock().expect("pool lock").len()
    }
}

impl<T> fmt::Debug for Pool<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("capacity", &self.capacity())
            .field("available", &self.available())
            .finish()
    }
}

/// An item borrowed from a [`Pool`]; returned to it on drop. If the pool
/// itself is gone (e.g. replaced after a resize), the item is simply dropped.
pub struct Lease<T> {
    item: Option<T>,
    pool: Arc<PoolInner<T>>,
}

impl<T> Deref for Lease<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.item.as_ref().expect("lease holds its item until drop")
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            self.pool.free.lock().expect("pool lock").push(item);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Lease<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Lease").field(&**self).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_return_items_on_drop() {
        let pool = Pool::new(vec![1, 2]);
        let first = pool.try_acquire().unwrap();
        assert_eq!(pool.available(), 1);
        drop(first);
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.capacity(), 2);
    }

    #[test]
    fn exhausted_pool_refuses_without_growing() {
        let pool = Pool::new(vec!['a', 'b']);
        let held: Vec<_> = (0..2).map(|_| pool.try_acquire().unwrap()).collect();
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.capacity(), 2);
        drop(held);
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn lease_outliving_its_pool_is_dropped_cleanly() {
        let pool = Pool::new(vec![String::from("texture")]);
        let lease = pool.try_acquire().unwrap();
        drop(pool);
        assert_eq!(&*lease, "texture");
        drop(lease);
    }

    #[test]
    fn leases_can_be_returned_from_another_thread() {
        let pool = Pool::new(vec![7_u32]);
        let lease = pool.try_acquire().unwrap();
        std::thread::spawn(move || drop(lease)).join().unwrap();
        assert_eq!(pool.available(), 1);
    }
}
