/// FlatStorage works similar to map. Each element is assigned an ID/key (or index in Vec) of usize type when inserting to FlatStorage.
/// The ID/key can be used to read/modify/remove the element.
/// Users can use it to create link list or just use it as a container.
pub struct FlatStorage<T> {
    data: Vec<AllocNode<T>>,
    count: usize,
    free: usize, // free list head
}

const INVALID_ID: usize = usize::MAX;

enum AllocNode<T> {
    Vacant(usize), // next slot index
    Occupied(T),
}

impl<T> Default for FlatStorage<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FlatStorage<T> {
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            count: 0,
            free: INVALID_ID,
        }
    }
    /// retrun the number of saved elements.
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn count_free(&self) -> usize {
        self.data.len() - self.count
    }
    pub fn capacity(&self) -> usize {
        self.data.len()
    }
    /// return the key assigned to new added element.
    pub fn add(&mut self, val: T) -> usize {
        self.count += 1;
        if self.free == INVALID_ID {
            self.data.push(AllocNode::<T>::Occupied(val));
            self.data.len() - 1
        } else {
            let key = self.free;
            match self.data[key] {
                AllocNode::<T>::Vacant(next) => {
                    self.free = next;
                }
                AllocNode::<T>::Occupied(_) => {
                    panic!("Expecting vacant slot pointed by free list.");
                }
            }
            self.data[key] = AllocNode::<T>::Occupied(val);
            key
        }
    }

    pub fn remove(&mut self, key: usize) -> Option<T> {
        if key < self.data.len() {
            if let AllocNode::<T>::Vacant(_) = self.data[key] {
                return None;
            } else if let AllocNode::<T>::Occupied(val) =
                std::mem::replace(&mut self.data[key], AllocNode::<T>::Vacant(self.free))
            {
                self.free = key;
                self.count -= 1;
                return Some(val);
            }
        }
        None
    }

    pub fn get(&self, key: usize) -> Option<&T> {
        if key < self.data.len() {
            if let AllocNode::<T>::Occupied(ref val) = self.data[key] {
                return Some(val);
            }
        }
        None
    }
    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        if key < self.data.len() {
            if let AllocNode::<T>::Occupied(ref mut val) = self.data[key] {
                return Some(val);
            }
        }
        None
    }
}

#[cfg(test)]
pub mod test {
    // use super::*;
    #[test]
    pub fn test_flat_storage() {
        assert_eq!(2, 2);
    }
}
