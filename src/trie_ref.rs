
use core::slice;
use core::mem::MaybeUninit;
use std::sync::Arc;

use crate::alloc::{global_alloc, Allocator, GlobalAlloc};
use crate::utils::ByteMask;
use crate::PathMap;
use crate::trie_node::*;
use crate::zipper::*;
use crate::zipper::zipper_priv::*;
use crate::zipper_tracking::*;

/// A borrowed read-only reference to a location in a trie
///
/// `TrieRef`s are like a read-only [Zipper] that can't move, but it is *much* smaller, cheaper to
/// create, and cheaper to work with.
//
//TODO: If we want to get a `TrieRef` down a little smaller, we can push the tag from [TaggedNodeRef]
// into 3 or 4 unused bits in the pointer.  (i.e. convert it to a SlimPtr)
//
//TODO: We could go even further towards shrinking the `TrieRef` and ultimately get down to 16 Bytes
// total by modifying the TrieNode API to create a "location_ref".  That would look roughly like this:
//
// - The `node_contains_partial_key` method would be expanded into something like:
//  `fn location_for_partial_key(&self, key: &[u8]) -> u64;`  The returned location token
//  would uniquely identify any existing location within the node.
//
// - The returned location token needs to be able to represent any location within the node,
//  not just endpoints.  For a ByteNode, it's easy, it's just a sentinel to indicate pointing
//  at the base of the node, and a u8 for positions one byte below the base.  For the ListNode/
//  aka PairNode, the logic would probably be a bit to indicate whether we're looking at slot0
//  or slot1, and a counter to indicate how many bytes into that slot's key we are pointing at.
//
// - There would be universally recognized sentinel value (universal across all node types) for
//   non-existant paths, which would be the equivalent to `node_contains_partial_key` returning false.
//
// - We'd want another method to reverse the transformation from location token back into a path,
//   This would be needed to initialize the path of a zipper, forked from a TrieRef, and likely
//   elsewhere too if we used location tokens extensively inside the zipper.
//
// - Many accessor functions could take a location token instead of a node_key.  For example,
//   `count_branches`, `node_branches_mask`, `get_node_at_key` (may rename it), `get_node_at_key`,
//   `node_get_child`
//
// - Implementing this change above may have some interactions with `tiny_node` / TinyRefNode,
//   it could lead to some potential simplifications to the code overall, if we can come up with
//   an interface that allows any subtrie within a node to function as a node unto itself.
//   Because currently `TinyRefNode` targets a special case, so we need a fallback for when
//   that case doesn't apply.
//
pub struct TrieRef<'a, V: Clone + Send + Sync, A: Allocator = GlobalAlloc> {
    focus_node: TaggedNodeRef<'a, V, A>,
    val_or_key: ValRefOrKey<'a, V>,
    alloc: A
}

impl<V: Clone + Send + Sync> Default for TrieRef<'_, V> {
    fn default() -> Self {
        Self::new_invalid_in(global_alloc())
    }
}

/// A TrieRef will never have both a non-zero-len node_key and a non-None value reference
#[repr(C)]
union ValRefOrKey<'a, V> {
    /// A length byte, followed by the key bytes themselves
    node_key: (u8, [MaybeUninit<u8>; MAX_NODE_KEY_BYTES]),
    /// VAL_REF_SENTINEL, followed by the reference to the value
    val_ref: (u64, Option<&'a V>)
}

/// Marks the first part of the `val_ref` variant of the [ValRefOrKey] enum.  This will never occur
/// by accident because the length byte at the beginning of the `node_key` variant has limited range
const VAL_REF_SENTINEL: u64 = 0xFFFFFFFFFFFFFFFF;

/// Marks a [TrieRef] that is invalid.  Same logic as VAL_REF_SENTINEL regarding accidental collision
const BAD_SENTINEL: u64 = 0xFEFEFEFEFEFEFEFE;

impl<V> Clone for ValRefOrKey<'_, V> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for ValRefOrKey<'_, V> {}

impl<V: Clone + Send + Sync, A: Allocator> Clone for TrieRef<'_, V, A> {
    #[inline]
    fn clone(&self) -> Self {
        Self{
            focus_node: self.focus_node,
            val_or_key: self.val_or_key,
            alloc: self.alloc.clone(),
        }
    }
}
impl<V: Clone + Send + Sync, A: Allocator + Copy> Copy for TrieRef<'_, V, A> {}

impl<'a, V: Clone + Send + Sync + 'a, A: Allocator + 'a> TrieRef<'a, V, A> {
    /// Makes a new sentinel that points to nothing.  THe allocator is just to keep the type system happy
    pub(crate) fn new_invalid_in(alloc: A) -> Self {
        Self { focus_node: TaggedNodeRef::empty_node(), val_or_key: ValRefOrKey { val_ref: (BAD_SENTINEL, None) }, alloc }
    }
    /// Internal constructor
    pub(crate) fn new_with_node_and_path_in(root_node: TaggedNodeRef<'a, V, A>, root_val: Option<&'a V>, path: &[u8], alloc: A) -> Self {
        let (node, key, val) = node_along_path(root_node, path, root_val);
        let node_key_len = key.len();
        let val_or_key = if node_key_len > 0 && node_key_len <= MAX_NODE_KEY_BYTES {
            let mut node_key_bytes = [MaybeUninit::uninit(); MAX_NODE_KEY_BYTES];
            unsafe {
                let src_ptr = key.as_ptr();
                let dst_ptr = node_key_bytes.as_mut_ptr().cast::<u8>();
                core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, node_key_len);
            }
            ValRefOrKey { node_key: (node_key_len as u8, node_key_bytes) }
        } else {
            if node_key_len <= MAX_NODE_KEY_BYTES {
                ValRefOrKey { val_ref: (VAL_REF_SENTINEL, val) }
            } else {
                ValRefOrKey { val_ref: (BAD_SENTINEL, None) }
            }
        };

        Self { focus_node: node, val_or_key, alloc }
    }
    /// Internal.  Checks if the `TrieRef` is valid, which is a prerequisite to see if it's pointing
    /// at an existing path
    #[inline]
    fn is_valid(&self) -> bool {
        (unsafe{ self.val_or_key.node_key.0 } != 0xFE)
    }
    /// Internal.  Gets the node key from the `TrieRef`
    #[inline]
    fn node_key(&self) -> &[u8] {
        let key_len = unsafe{ self.val_or_key.node_key.0 } as usize;
        if key_len > MAX_NODE_KEY_BYTES {
            &[]
        } else {
            unsafe{ slice::from_raw_parts(self.val_or_key.node_key.1.as_ptr().cast(), key_len) }
        }
    }
    /// Internal.  Gets the root val from the `TrieRef`, which is `None` if the `TrieRef` has a [Self::node_key]
    #[inline]
    fn root_val(&self) -> Option<&'a V> {
        if unsafe{ self.val_or_key.val_ref.0 } == VAL_REF_SENTINEL {
            unsafe{ self.val_or_key.val_ref.1 }
        } else {
            None
        }
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> Zipper for TrieRef<'_, V, A> {
    fn path_exists(&self) -> bool {
        if self.is_valid() {
            let key = self.node_key();
            if key.len() > 0 {
                self.focus_node.node_contains_partial_key(key)
            } else {
                true
            }
        } else {
            false
        }
    }
    fn is_val(&self) -> bool {
        self.get_val().is_some()
    }
    fn child_count(&self) -> usize {
        if self.is_valid() {
            self.focus_node.count_branches(self.node_key())
        } else {
            0
        }
    }
    fn child_mask(&self) -> ByteMask {
        if self.is_valid() {
            self.focus_node.node_branches_mask(self.node_key())
        } else {
            ByteMask::EMPTY
        }
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperValues<V> for TrieRef<'_, V, A> {
    fn val(&self) -> Option<&V> {
        self.get_val()
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperForking<V> for TrieRef<'_, V, A> {
    type ReadZipperT<'a> = ReadZipperUntracked<'a, 'a, V, A> where Self: 'a;
    fn fork_read_zipper<'a>(&'a self) -> Self::ReadZipperT<'a> {
        let core_z = read_zipper_core::ReadZipperCore::new_with_node_and_path_internal_in(self.focus_node.clone(), self.node_key(), 0, self.get_val(), self.alloc.clone());
        Self::ReadZipperT::new_forked_with_inner_zipper(core_z)
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperSubtries<V, A> for TrieRef<'_, V, A> {
    fn make_map(&self) -> Option<PathMap<Self::V, A>> {
        #[cfg(not(feature = "graft_root_vals"))]
        let root_val = None;
        #[cfg(feature = "graft_root_vals")]
        let root_val = self.get_val().cloned();

        let root_node = self.get_focus().into_option();
        if root_node.is_some() || root_val.is_some() {
            Some(PathMap::new_with_root_in(root_node, root_val, self.alloc.clone()))
        } else {
            None
        }
    }
}

impl<V: Clone + Send + Sync, A: Allocator> zipper_priv::ZipperPriv for TrieRef<'_, V, A> {
    type V = V;
    type A = A;
    fn get_focus(&self) -> AbstractNodeRef<'_, Self::V, Self::A> {
        if self.is_valid() {
            self.focus_node.get_node_at_key(self.node_key())
        } else {
            AbstractNodeRef::None
        }
    }
    fn try_borrow_focus(&self) -> Option<TaggedNodeRef<'_, Self::V, Self::A>> {
        if self.is_valid() {
            let node_key = self.node_key();
            if node_key.len() == 0 {
                Some(self.focus_node)
            } else {
                match self.focus_node.node_get_child(node_key) {
                    Some((consumed_bytes, child_node)) => {
                        let child_node = child_node.as_tagged();
                        debug_assert_eq!(consumed_bytes, node_key.len());
                        Some(child_node)
                    },
                    None => None
                }
            }
        } else {
            None
        }
    }
}

impl<'a, V: Clone + Send + Sync + Unpin + 'a, A: Allocator + 'a> ZipperReadOnlyValues<'a, V> for TrieRef<'a, V, A> {
    #[inline]
    fn get_val(&self) -> Option<&'a V> {
        if self.is_valid() {
            let key = self.node_key();
            if key.len() > 0 {
                self.focus_node.node_get_val(key)
            } else {
                unsafe{
                    debug_assert_eq!(self.val_or_key.val_ref.0, VAL_REF_SENTINEL);
                    self.val_or_key.val_ref.1
                }
            }
        } else {
            None
        }
    }
}

impl<'a, V: Clone + Send + Sync + Unpin + 'a, A: Allocator + 'a> ZipperReadOnlySubtries<'a, V, A> for TrieRef<'a, V, A> {
    fn trie_ref_at_path<K: AsRef<[u8]>>(&self, path: K) -> TrieRef<'a, V, A> {
        if self.is_valid() {
            let path = path.as_ref();
            let node_key = self.node_key();
            if node_key.len() > 0 {
                trie_ref_at_path_in(self.focus_node, None, node_key, path, self.alloc.clone())
            } else {
                trie_ref_at_path_in(self.focus_node, self.root_val(), &[], path, self.alloc.clone())
            }
        } else {
            TrieRef::new_invalid_in(self.alloc.clone())
        }
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperConcrete for TrieRef<'_, V, A> {
    fn is_shared(&self) -> bool {
        false //We don't have enough info in the TrieRef to get back to the Rc header.  This may change in the future
    }
}

impl<'a, V: Clone + Send + Sync + Unpin, A: Allocator + 'a> ZipperReadOnlyPriv<'a, V, A> for TrieRef<'a, V, A> {
    fn borrow_raw_parts<'z>(&'z self) -> (TaggedNodeRef<'a, V, A>, &'z [u8], Option<&'a V>) {
        (self.focus_node, self.node_key(), self.root_val())
    }
    fn take_core(&mut self) -> Option<read_zipper_core::ReadZipperCore<'a, 'static, V, A>> {
        None
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperConcretePriv for TrieRef<'_, V, A> {
    fn shared_node_id(&self) -> Option<u64> { read_zipper_core::read_zipper_shared_node_id(self) }
}

/// Internal function to implement [ZipperReadOnly::trie_ref_at_path] for all the types that need it
pub(crate) fn trie_ref_at_path_in<'a, 'paths, V: Clone + Send + Sync, A: Allocator + 'a>(mut node: TaggedNodeRef<'a, V, A>, root_val: Option<&'a V>, node_key: &'paths [u8], mut path: &'paths [u8], alloc: A) -> TrieRef<'a, V, A> {

    // A temporary buffer on the stack, if we need to assemble a combined key from both the `node_key` and `path`
    let mut temp_key_buf: [MaybeUninit<u8>; MAX_NODE_KEY_BYTES] = [MaybeUninit::uninit(); MAX_NODE_KEY_BYTES];

    let node_key_len = node_key.len();
    let path_len = path.len();

    //Copy the existing node key and the first chunk of the path into the temp buffer, and try to
    // descend one step
    if node_key_len > 0 && path_len > 0 {
        let next_node_path = unsafe {
            let src_ptr = node_key.as_ptr();
            let dst_ptr = temp_key_buf.as_mut_ptr().cast::<u8>();
            core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, node_key_len);

            let remaining_len = (MAX_NODE_KEY_BYTES - node_key_len).min(path_len);
            let src_ptr = path.as_ptr();
            let dst_ptr = temp_key_buf.as_mut_ptr().cast::<u8>().add(node_key_len);
            core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, remaining_len);

            let total_buf_len = node_key_len + remaining_len;
            core::slice::from_raw_parts(temp_key_buf.as_mut_ptr().cast::<u8>(), total_buf_len)
        };

        if let Some((consumed_byte_cnt, next_node)) = node.node_get_child(next_node_path) {
            let next_node = next_node.as_tagged();
            debug_assert!(consumed_byte_cnt >= node_key_len);
            node = next_node;
            path = &path[consumed_byte_cnt-node_key_len..];
        } else {
            path = next_node_path;
        }
    } else {
        if path_len == 0 {
            path = node_key;
        }
    }

    //Descend the rest of the way along the path
    let (node, key, val) = node_along_path(node, path, root_val);

    TrieRef::new_with_node_and_path_in(node, val, key, alloc)
}

/// A borrowed read-only reference to a location in a trie.  See [TrieRef]
///
#[derive(Clone)]
pub struct TrieRefTracked<'a, V: Clone + Send + Sync, A: Allocator = GlobalAlloc> {
    pub(crate) trie_ref: TrieRef<'a, V, A>,
    pub(crate) tracker: Arc<ZipperTracker<TrackingRead>>,
}

impl<'a, V: Clone + Send + Sync, A: Allocator> Drop for TrieRefTracked<'a, V, A> {
    fn drop(&mut self) { }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> Zipper for TrieRefTracked<'_, V, A> {
    fn path_exists(&self) -> bool { self.trie_ref.path_exists() }
    fn is_val(&self) -> bool { self.trie_ref.is_val() }
    fn child_count(&self) -> usize { self.trie_ref.child_count() }
    fn child_mask(&self) -> ByteMask { self.trie_ref.child_mask() }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperValues<V> for TrieRefTracked<'_, V, A> {
    fn val(&self) -> Option<&V> { self.trie_ref.val() }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperForking<V> for TrieRefTracked<'_, V, A> {
    type ReadZipperT<'a> = ReadZipperUntracked<'a, 'a, V, A> where Self: 'a;
    fn fork_read_zipper<'a>(&'a self) -> Self::ReadZipperT<'a> { self.trie_ref.fork_read_zipper() }
}

impl<V: Clone + Send + Sync, A: Allocator> zipper_priv::ZipperPriv for TrieRefTracked<'_, V, A> {
    type V = V;
    type A = A;
    fn get_focus(&self) -> AbstractNodeRef<'_, Self::V, Self::A> { self.trie_ref.get_focus() }
    fn try_borrow_focus(&self) -> Option<TaggedNodeRef<'_, Self::V, Self::A>> { self.trie_ref.try_borrow_focus() }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperSubtries<V, A> for TrieRefTracked<'_, V, A> {
    fn make_map(&self) -> Option<PathMap<Self::V, A>> { self.trie_ref.make_map() }
}

impl<'a, V: Clone + Send + Sync + Unpin + 'a, A: Allocator + 'a> ZipperReadOnlyConditionalSubtries<'a, V, A> for TrieRefTracked<'a, V, A> {
    fn trie_ref_at_path<K: AsRef<[u8]>>(&self, path: K) -> TrieRefTracked<'a, V, A> {
        let inner = self.trie_ref.trie_ref_at_path(path);
        TrieRefTracked{ trie_ref: inner, tracker: self.tracker.clone() }
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperConcrete for TrieRefTracked<'_, V, A> {
    fn is_shared(&self) -> bool {
        false //We don't have enough info in the TrieRef to get back to the Rc header.  This may change in the future
    }
}

impl<'a, V: Clone + Send + Sync + Unpin, A: Allocator + 'a> ZipperReadOnlyPriv<'a, V, A> for TrieRefTracked<'a, V, A> {
    fn borrow_raw_parts<'z>(&'z self) -> (TaggedNodeRef<'a, V, A>, &'z [u8], Option<&'a V>) { self.trie_ref.borrow_raw_parts() }
    fn take_core(&mut self) -> Option<read_zipper_core::ReadZipperCore<'a, 'static, V, A>> { None }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> ZipperConcretePriv for TrieRefTracked<'_, V, A> {
    fn shared_node_id(&self) -> Option<u64> { self.trie_ref.shared_node_id() }
}

#[cfg(test)]
mod tests {
    use crate::{alloc::global_alloc, PathMap, utils::ByteMask, zipper::*};

    #[test]
    fn trie_ref_test1() {

        let keys = ["Hello", "Hell", "Help", "Helsinki"];
        let map: PathMap<()> = keys.iter().map(|k| (k, ())).collect();

        // With the current node types, there likely isn't any reason the node would be split at "He"
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"He", global_alloc());
        #[cfg(not(feature = "all_dense_nodes"))]
        assert_eq!(trie_ref.node_key(), b"He");
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), true);

        // "Hel" on the other hand was likely split into its own node
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"Hel", global_alloc());
        assert_eq!(trie_ref.node_key().len(), 0);
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), true);

        // Make sure we get a value at a leaf
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"Help", global_alloc());
        assert_eq!(trie_ref.get_val(), Some(&()));
        assert_eq!(trie_ref.path_exists(), true);

        // Make sure we get a value in the middle of a path
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"Hell", global_alloc());
        assert_eq!(trie_ref.get_val(), Some(&()));
        assert_eq!(trie_ref.path_exists(), true);

        // Try a path that doesn't exist
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"Hi", global_alloc());
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), false);

        // Try a very long path that doesn't exist but is sure to blow the single-node path buffer
        let trie_ref = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"Hello Mr. Washington, my name is John, but sometimes people call me Jack.  I live in Springfield.", global_alloc());
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), false);

        //Try using TrieRefs to descend a trie
        let trie_ref0 = TrieRef::new_with_node_and_path_in(map.root().unwrap().as_tagged(), map.root_val(), b"H", global_alloc());
        assert_eq!(trie_ref0.get_val(), None);
        assert_eq!(trie_ref0.path_exists(), true);
        assert_eq!(trie_ref0.child_count(), 1);
        assert_eq!(trie_ref0.child_mask(), ByteMask::from(b'e'));

        //"Hel"
        let trie_ref1 = trie_ref0.trie_ref_at_path(b"el");
        assert_eq!(trie_ref1.get_val(), None);
        assert_eq!(trie_ref1.path_exists(), true);
        assert_eq!(trie_ref1.child_count(), 3);

        //"Hello"
        let trie_ref2 = trie_ref1.trie_ref_at_path(b"lo");
        assert_eq!(trie_ref2.get_val(), Some(&()));
        assert_eq!(trie_ref2.path_exists(), true);
        assert_eq!(trie_ref2.child_count(), 0);

        //"HelloOperator"
        let trie_ref3 = trie_ref2.trie_ref_at_path(b"Operator");
        assert_eq!(trie_ref3.get_val(), None);
        assert_eq!(trie_ref3.path_exists(), false);
        assert_eq!(trie_ref3.child_count(), 0);

        //"HelloOperator, give me number 9"
        let trie_ref4 = trie_ref3.trie_ref_at_path(b", give me number 9");
        assert_eq!(trie_ref4.get_val(), None);
        assert_eq!(trie_ref4.path_exists(), false);
        assert_eq!(trie_ref4.child_count(), 0);

        //"Hell"
        let trie_ref2 = trie_ref1.trie_ref_at_path(b"l");
        assert_eq!(trie_ref2.get_val(), Some(&()));
        assert_eq!(trie_ref2.path_exists(), true);
        assert_eq!(trie_ref2.child_count(), 1);
    }

    #[test]
    fn trie_ref_test2() {
        let rs = ["arrow", "bow", "cannon", "roman", "romane", "romanus^", "romulus", "rubens", "ruber", "rubicon", "rubicundus", "rom'i"];
        let btm: PathMap<usize> = rs.into_iter().enumerate().map(|(i, r)| (r.as_bytes(), i) ).collect();

        let trie_ref = btm.trie_ref_at_path([]);
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), true);
        assert_eq!(trie_ref.child_count(), 4);

        let trie_ref = trie_ref.trie_ref_at_path([b'a']);
        assert_eq!(trie_ref.get_val(), None);
        assert_eq!(trie_ref.path_exists(), true);
        assert_eq!(trie_ref.child_count(), 1);

        let trie_ref = btm.trie_ref_at_path(b"r");
        let mut z = trie_ref.fork_read_zipper();
        assert_eq!(z.val_count(), 9);
        z.descend_to(b"om");
        assert_eq!(z.val_count(), 5);
        assert_eq!(z.path(), b"om");
        assert_eq!(z.origin_path(), b"om");

        let new_map = trie_ref.make_map().unwrap();
        assert_eq!(new_map.val_count(), 9);
    }

    /// Tests that a TrieRef can't be invalidated through a ZipperHead
    #[test]
    fn trie_ref_test3() {
        let mut map = PathMap::<usize>::new();
        map.set_val_at(b"path", 42);
        let zh = map.zipper_head();
        let rz = zh.read_zipper_at_borrowed_path(b"path").unwrap();
        let tr = rz.trie_ref_at_path(b"");

        assert_eq!(tr.val(), Some(&42));
        drop(rz);

        assert_eq!(tr.val(), Some(&42));
        assert!(zh.write_zipper_at_exclusive_path(b"path").is_err());

        assert_eq!(tr.val(), Some(&42));
        drop(tr);

        let mut wz = zh.write_zipper_at_exclusive_path(b"path").unwrap();
        wz.remove_val();
        drop(wz);
        drop(zh);
        assert_eq!(map.get_val_at(b"path"), None);
    }
}
