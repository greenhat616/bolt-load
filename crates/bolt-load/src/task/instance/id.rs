//! This module provides a efficient ID generator for managing task ID allocation

use bitvec::prelude::*;

/// Efficient ID generator for managing task ID allocation
///
/// Uses BitVec to track ID usage and maintains a hint pointer for performance optimization
pub struct Generator {
    /// Bit vector for tracking ID usage status
    bitvec: BitVec,
    /// Hint pointer to the next possible free position for search optimization
    next_free_hint: usize,
    /// Current count of allocated IDs
    allocated_count: usize,
}

impl Generator {
    /// Create a new ID generator with specified capacity
    ///
    /// # Arguments
    /// * `capacity` - Maximum capacity of the ID generator
    ///
    /// # Example
    /// ```ignore
    /// let mut generator = Generator::new(100);
    /// ```
    pub fn new(capacity: usize) -> Self {
        Self {
            bitvec: bitvec![0; capacity],
            next_free_hint: 0,
            allocated_count: 0,
        }
    }

    /// Get the next available ID
    ///
    /// Time complexity: Average O(1), worst case O(n)
    ///
    /// # Returns
    /// * `Some(id)` - If an ID is available
    /// * `None` - If all IDs are allocated
    #[inline]
    pub fn next(&mut self) -> Option<usize> {
        // Quick check if full
        if self.allocated_count >= self.bitvec.len() {
            return None;
        }

        // Search starting from hint position
        let capacity = self.bitvec.len();
        for i in 0..capacity {
            let index = (self.next_free_hint + i) % capacity;
            if !self.bitvec[index] {
                self.bitvec.set(index, true);
                self.allocated_count += 1;
                // Update hint pointer to next possible position
                self.next_free_hint = (index + 1) % capacity;
                return Some(index);
            }
        }

        None
    }

    /// Release the specified ID, making it available for reallocation
    ///
    /// # Arguments
    /// * `id` - The ID to release
    ///
    /// # Returns
    /// * `true` - If successfully released
    /// * `false` - If ID is invalid or already free
    #[inline]
    pub fn release(&mut self, id: usize) -> bool {
        if id >= self.bitvec.len() {
            return false;
        }

        if self.bitvec[id] {
            self.bitvec.set(id, false);
            self.allocated_count -= 1;
            // Update hint pointer to optimize next allocation
            if id < self.next_free_hint {
                self.next_free_hint = id;
            }
            true
        } else {
            false
        }
    }

    /// Check if the specified ID is allocated
    ///
    /// # Arguments
    /// * `id` - The ID to check
    ///
    /// # Returns
    /// * `true` - If the ID is allocated
    /// * `false` - If the ID is free or invalid
    #[inline]
    #[allow(dead_code)]
    pub fn is_allocated(&self, id: usize) -> bool {
        id < self.bitvec.len() && self.bitvec[id]
    }

    /// Get the current count of allocated IDs
    #[inline]
    pub fn allocated_count(&self) -> usize {
        self.allocated_count
    }

    /// Get the total capacity
    #[inline]
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.bitvec.len()
    }

    /// Check if the generator is full
    #[inline]
    pub fn is_full(&self) -> bool {
        self.allocated_count >= self.bitvec.len()
    }

    /// Check if the generator is empty (no IDs allocated)
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.allocated_count == 0
    }

    /// Reset the generator, releasing all allocated IDs
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.bitvec.fill(false);
        self.next_free_hint = 0;
        self.allocated_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_allocation() {
        let mut g = Generator::new(5);

        assert_eq!(g.next(), Some(0));
        assert_eq!(g.next(), Some(1));
        assert_eq!(g.allocated_count(), 2);
        assert!(!g.is_full());
    }

    #[test]
    fn test_release_and_reuse() {
        let mut g = Generator::new(3);

        let id1 = g.next().unwrap();
        let _id2 = g.next().unwrap();

        assert!(g.release(id1));
        assert!(!g.release(id1)); // Duplicate release should fail

        let id3 = g.next().unwrap();
        assert_eq!(id3, id1); // Should reuse the just released ID
    }

    #[test]
    fn test_capacity_limits() {
        let mut g = Generator::new(2);

        assert_eq!(g.next(), Some(0));
        assert_eq!(g.next(), Some(1));
        assert_eq!(g.next(), None); // Should be full
        assert!(g.is_full());

        g.release(0);
        assert!(!g.is_full());
        assert_eq!(g.next(), Some(0));
    }

    #[test]
    fn test_reset() {
        let mut g = Generator::new(3);
        let _ = g.next();
        let _ = g.next();

        assert_eq!(g.allocated_count(), 2);

        g.reset();
        assert_eq!(g.allocated_count(), 0);
        assert!(g.is_empty());
        assert_eq!(g.next(), Some(0));
    }
}
