use core::cell::UnsafeCell;
use std::ptr::slice_from_raw_parts;
use crate::alloc::{Allocator, GlobalAlloc, global_alloc};
use crate::morphisms::{new_map_from_ana_in, Catamorphism, TrieBuilder};
use crate::trie_node::*;
use crate::zipper::*;
use crate::ring::{AlgebraicResult, AlgebraicStatus, COUNTER_IDENT, SELF_IDENT, Lattice, LatticeRef, DistributiveLattice, DistributiveLatticeRef, Quantale};

#[cfg(not(miri))]
use gxhash::gxhash128;

#[cfg(miri)]
fn gxhash128(data: &[u8], _seed: i64) -> u128 { xxhash_rust::const_xxh3::xxh3_128(data) }

//GOAT-old-names
#[deprecated]
pub type BytesTrieMap<V, A = GlobalAlloc> = PathMap<V, A>;

/// A map type that uses byte slices `&[u8]` as keys
///
/// This type is implemented using some of the approaches explained in the
/// ["Bitwise trie with bitmap" Wikipedia article](https://en.wikipedia.org/wiki/Bitwise_trie_with_bitmap).
///
/// ```
/// # use pathmap::PathMap;
/// let mut map = PathMap::<String>::new();
/// map.set_val_at("one", "1".to_string());
/// map.set_val_at("two", "2".to_string());
///
/// assert!(map.contains("one"));
/// assert_eq!(map.get("two"), Some(&"2".to_string()));
/// assert!(!map.contains("three"));
/// ```
pub struct PathMap<
    V: Clone + Send + Sync,
    A: Allocator = GlobalAlloc,
> {
    pub(crate) root: UnsafeCell<Option<TrieNodeODRc<V, A>>>,
    pub(crate) root_val: UnsafeCell<Option<V>>,
    pub(crate) alloc: A,
}

unsafe impl<V: Clone + Send + Sync, A: Allocator> Send for PathMap<V, A> {}
unsafe impl<V: Clone + Send + Sync, A: Allocator> Sync for PathMap<V, A> {}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> Clone for PathMap<V, A> {
    fn clone(&self) -> Self {
        let root_ref = unsafe{ &*self.root.get() };
        let root_val_ref = unsafe{ &*self.root_val.get() };
        Self::new_with_root_in(root_ref.clone(), root_val_ref.clone(), self.alloc.clone())
    }
}

impl<V: Clone + Send + Sync + Unpin> PathMap<V, GlobalAlloc> {
    /// Creates a new empty map
    #[inline]
    pub const fn new() -> Self {
        Self::new_with_root_in(None, None, global_alloc())
    }

    /// Creates a new single-element pathmap
    #[inline]
    pub fn single<P: AsRef<[u8]>>(path: P, val: V) -> Self {
        Self::from((path, val))
    }

    /// Creates a new `PathMap` by evaluating the specified anamorphism
    ///
    /// `alg_f`: `alg(w: W, val: &mut Option<V>, children: &mut ChildBuilder<W>, path: &[u8])`
    /// generates the value downstream and downstream children from a path
    ///
    /// Setting the `val` option to `Some` within the closure sets the value at the current path.
    ///
    /// The example below creates a trie with binary tree, 3 levels deep, where each level has a 'L'
    /// and an 'R' branch, and the leaves have a unit value.
    /// ```
    /// # use pathmap::PathMap;
    /// let map = PathMap::<()>::new_from_ana(3, |idx, val, children, _path| {
    ///     if idx > 0 {
    ///         children.push(b"L", idx - 1);
    ///         children.push(b"R", idx - 1);
    ///     } else {
    ///         *val = Some(());
    ///     }
    /// });
    /// ```
    pub fn new_from_ana<W, AlgF>(w: W, alg_f: AlgF) -> Self
        where
        V: 'static,
        W: Default,
        AlgF: FnMut(W, &mut Option<V>, &mut TrieBuilder<V, W, GlobalAlloc>, &[u8])
    {
        Self::new_from_ana_in(w, alg_f, global_alloc())
    }

}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> PathMap<V, A> {
    #[inline]
    pub(crate) fn root(&self) -> Option<&TrieNodeODRc<V, A>> {
        unsafe{ &*self.root.get() }.as_ref()
    }
    #[inline]
    pub(crate) fn root_val(&self) -> Option<&V> {
        unsafe{ &*self.root_val.get() }.as_ref()
    }
    #[inline]
    pub(crate) fn root_val_mut(&mut self) -> &mut Option<V> {
        unsafe{ &mut *self.root_val.get() }
    }
    #[inline]
    pub(crate) fn get_or_init_root_mut(&mut self) -> &mut TrieNodeODRc<V, A> {
        self.ensure_root();
        self.root.get_mut().as_mut().unwrap()
    }
    /// Internal method to ensure there is a valid node at the root of the map
    #[inline]
    pub(crate) fn ensure_root(&self) {
        let root_ref = unsafe{ &*self.root.get() };
        if root_ref.is_some() {
            return
        }
        self.do_init_root();
    }
    #[cold]
    fn do_init_root(&self) {
        #[cfg(feature = "all_dense_nodes")]
        let root = TrieNodeODRc::new_in(crate::dense_byte_node::DenseByteNode::<V, A>::new_in(self.alloc.clone()), self.alloc.clone());
        #[cfg(feature = "bridge_nodes")]
        let root = TrieNodeODRc::new_in(crate::bridge_node::BridgeNode::new_empty(), self.alloc.clone());
        #[cfg(not(any(feature = "all_dense_nodes", feature = "bridge_nodes")))]
        let root = TrieNodeODRc::new_in(crate::line_list_node::LineListNode::new_in(self.alloc.clone()), self.alloc.clone());

        let root_ref = unsafe{ &mut *self.root.get() };
        *root_ref = Some(root);
    }

    /// Creates a new empty map in the specified allocator
    #[inline]
    pub const fn new_in(alloc: A) -> Self {
        Self::new_with_root_in(None, None, alloc)
    }

    /// Creates a new single-element pathmap in the specified allocator
    #[inline]
    pub fn single_in<P: AsRef<[u8]>>(path: P, val: V, alloc: A) -> Self {
        let mut map = Self::new_in(alloc);
        map.set_val_at(path, val);
        map
    }

    /// See [`new_from_ana`](Self::new_from_ana) for description of behavior
    pub fn new_from_ana_in<W, AlgF>(w: W, alg_f: AlgF, alloc: A) -> Self
        where
        V: 'static,
        W: Default,
        AlgF: FnMut(W, &mut Option<V>, &mut TrieBuilder<V, W, A>, &[u8])
    {
        new_map_from_ana_in(w, alg_f, alloc)
    }

    /// Internal Method.  Creates a new `PathMap` with the supplied root node
    #[inline]
    pub(crate) const fn new_with_root_in(
        root_node: Option<TrieNodeODRc<V, A>>,
        root_val: Option<V>,
        alloc: A
    ) -> Self {
        Self {
            root: UnsafeCell::new(root_node),
            root_val: UnsafeCell::new(root_val),
            alloc
        }
    }

    /// Internal Method.  Removes and returns the root node and root_val from a `PathMap`
    #[inline]
    pub(crate) fn into_root(self) -> (Option<TrieNodeODRc<V, A>>, Option<V>) {
        let root_node = match self.root() {
            Some(root) => if !root.as_tagged().node_is_empty() {
                self.root.into_inner()
            } else {
                None
            },
            None => None
        };
        let root_val = self.root_val.into_inner();
        (root_node, root_val)
    }

    /// Creates a new [TrieRef], referring to a position from the root of the `PathMap`
    pub fn trie_ref_at_path<K: AsRef<[u8]>>(&self, path: K) -> TrieRefBorrowed<'_, V, A> {
        self.ensure_root();
        let path = path.as_ref();
        TrieRefBorrowed::new_with_key_and_path_in(self.root().unwrap(), self.root_val(), &[], path, self.alloc.clone())
    }

    /// Creates a new read-only [Zipper], starting at the root of a `PathMap`
    pub fn read_zipper<'a>(&'a self) -> ReadZipperUntracked<'a, 'static, V, A> {
        self.ensure_root();
        let root_val = unsafe{ &*self.root_val.get() }.as_ref();
        #[cfg(debug_assertions)]
        {
            ReadZipperUntracked::new_with_node_and_path_internal_in(self.root().unwrap(), &[], 0, root_val, self.alloc.clone(), None)
        }
        #[cfg(not(debug_assertions))]
        {
            ReadZipperUntracked::new_with_node_and_path_internal_in(self.root().unwrap(), &[], 0, root_val, self.alloc.clone())
        }
    }

    /// Creates a new read-only [Zipper], with the specified path from the root of the map; This method is much more
    /// efficient than [read_zipper_at_path](Self::read_zipper_at_path), but means the resulting zipper is bound by
    /// the `'path` lifetime
    pub fn read_zipper_at_borrowed_path<'path>(&self, path: &'path[u8]) -> ReadZipperUntracked<'_, 'path, V, A> {
        self.ensure_root();
        let root_val = match path.len() == 0 {
            true => unsafe{ &*self.root_val.get() }.as_ref(),
            false => None
        };
        #[cfg(debug_assertions)]
        {
            ReadZipperUntracked::new_with_node_and_path_in(self.root().unwrap(), false, path.as_ref(), path.len(), 0, root_val, self.alloc.clone(), None)
        }
        #[cfg(not(debug_assertions))]
        {
            ReadZipperUntracked::new_with_node_and_path_in(self.root().unwrap(), false, path.as_ref(), path.len(), 0, root_val, self.alloc.clone())
        }
    }

    /// Creates a new read-only [Zipper], with the `path` specified from the root of the map
    pub fn read_zipper_at_path<K: AsRef<[u8]>>(&self, path: K) -> ReadZipperUntracked<'_, 'static, V, A> {
        self.ensure_root();
        let path = path.as_ref();
        let root_val = match path.len() == 0 {
            true => unsafe{ &*self.root_val.get() }.as_ref(),
            false => None
        };
        #[cfg(debug_assertions)]
        {
            ReadZipperUntracked::new_with_node_and_cloned_path_in(self.root().unwrap(), path, path.len(), 0, root_val, self.alloc.clone(), None)
        }
        #[cfg(not(debug_assertions))]
        {
            ReadZipperUntracked::new_with_node_and_cloned_path_in(self.root().unwrap(), path, path.len(), 0, root_val, self.alloc.clone())
        }
    }

    /// Creates a new [write zipper](ZipperWriting) starting at the root of the `PathMap`
    pub fn write_zipper(&mut self) -> WriteZipperUntracked<'_, 'static, V, A> {
        self.ensure_root();
        let root_node = self.root.get_mut().as_mut().unwrap();
        let root_val = self.root_val.get_mut();
        #[cfg(debug_assertions)]
        {
            WriteZipperUntracked::new_with_node_and_path_internal_in(root_node, Some(root_val), &[], 0, self.alloc.clone(), None)
        }
        #[cfg(not(debug_assertions))]
        {
            WriteZipperUntracked::new_with_node_and_path_internal_in(root_node, Some(root_val), &[], 0, self.alloc.clone())
        }
    }

    /// Creates a new [write zipper](ZipperWriting) with the specified path from the root of the map
    pub fn write_zipper_at_path<'a, 'path>(&'a mut self, path: &'path[u8]) -> WriteZipperUntracked<'a, 'path, V, A> {
        self.ensure_root();
        let root_node = self.root.get_mut().as_mut().unwrap();
        let root_val = match path.len() == 0 {
            true => Some(self.root_val.get_mut()),
            false => None
        };
        #[cfg(debug_assertions)]
        {
            WriteZipperUntracked::new_with_node_and_path_in(root_node, root_val, path, path.len(), 0, self.alloc.clone(), None)
        }
        #[cfg(not(debug_assertions))]
        {
            WriteZipperUntracked::new_with_node_and_path_in(root_node, root_val, path, path.len(), 0, self.alloc.clone())
        }
    }

    /// Creates a [ZipperHead] at the root of the map
    pub fn zipper_head(&mut self) -> ZipperHead<'_, '_, V, A> {
        self.ensure_root();
        let root_node = self.root.get_mut().as_mut().unwrap();
        let root_val = self.root_val.get_mut();
        let z = WriteZipperCore::new_with_node_and_path_internal_in(root_node, Some(root_val), &[], 0, self.alloc.clone());
        z.into_zipper_head()
    }

    /// Transforms the map into a [Zipper], which is handy when you need to embed the zipper in another
    /// struct without a lifetime parameter
    pub fn into_read_zipper<K: AsRef<[u8]>>(self, path: K) -> ReadZipperOwned<V, A> {
        ReadZipperOwned::new_with_map(self, path)
    }

    /// Transforms the map into a [WriteZipperOwned], which is handy when you need to embed the zipper
    /// in another struct without a lifetime parameter
    pub fn into_write_zipper<K: AsRef<[u8]>>(self, path: K) -> WriteZipperOwned<V, A> {
        WriteZipperOwned::new_with_map(self, path)
    }

    /// Transforms the map into a [ZipperHead] that owns the map's contents.  This is handy when the
    /// ZipperHead needs to be part of another structure
    pub fn into_zipper_head<K: AsRef<[u8]>>(self, path: K) -> ZipperHeadOwned<V, A> {
        let path = path.as_ref();
        let mut wz = self.into_write_zipper(&path);
        if path.len() > 0 {
            wz.core().key.prepare_buffers();
        }
        ZipperHeadOwned::new(wz)
    }

    /// Returns an iterator over all key-value pairs within the map
    ///
    /// NOTE: This is much less efficient than using the [read_zipper](Self::read_zipper) method
    pub fn iter<'a>(&'a self) -> impl Iterator<Item=(Vec<u8>, &'a V)> + 'a {
        self.read_zipper().into_iter()
    }

    /// Returns `true` if the map contains a value at the specified key, otherwise returns `false`
    pub fn contains<K: AsRef<[u8]>>(&self, k: K) -> bool {
        let k = k.as_ref();

        //NOTE: Here is the old impl traversing without the zipper.  The zipper version appears to be
        // nearly the same perf.  All averages within 3% in both directions, and the zipper impl being
        // faster as often as the native (non-zipper) version
        // let (node, remaining_key) = traverse_to_leaf(self.root.borrow(), k);
        // node.node_contains_val(remaining_key)

        let zipper = self.read_zipper_at_borrowed_path(k);
        zipper.is_val()
    }

    /// Returns `true` if a path is contained within the map, or `false` otherwise
    pub fn contains_path<K: AsRef<[u8]>>(&self, k: K) -> bool {
        let k = k.as_ref();
        let zipper = self.read_zipper_at_borrowed_path(k);
        zipper.path_exists()
    }

    /// Inserts `v` into the map at `path`.  Panics if `path` has a zero length
    ///
    /// Returns `Some(replaced_val)` if an existing value was replaced, otherwise returns `None` if
    /// the value was added to the map without replacing anything.
    pub fn set_val_at<K: AsRef<[u8]>>(&mut self, path: K, v: V) -> Option<V> {
        let path = path.as_ref();

        //NOTE: Here is the old impl traversing without the zipper.  Kept here for benchmarking purposes
        // However, the zipper version is basically identical performance, within the margin of error 
        // traverse_to_leaf_static_result(&mut self.root, k,
        // |node, remaining_key| node.node_set_val(remaining_key, v),
        // |_new_leaf_node, _remaining_key| None)

        let mut zipper = self.write_zipper_at_path(path);
        zipper.set_val(v)
    }

    /// Deprecated alias for [Self::set_val_at]
    #[deprecated] //GOAT-old-names
    pub fn insert<K: AsRef<[u8]>>(&mut self, k: K, v: V) -> Option<V> {
        self.set_val_at(k, v)
    }

    //GOAT, make a separate `join_val_at` that is similar to `set_val_at` except replaces V with a merged V rather
    // than replacing it

    /// Removes the value at `path` from the map and returns it, or returns `None` if there was no value at `path`
    pub fn remove_val_at<K: AsRef<[u8]>>(&mut self, path: K) -> Option<V> {
        let path = path.as_ref();
        //NOTE: we're descending the zipper rather than creating it at the path so it will be allowed to
        // prune the branches.  A WriteZipper can't move above its root, so it couldn't prune otherwise
        let mut zipper = self.write_zipper();
        zipper.descend_to(path);
        zipper.remove_val()
    }

    /// Deprecated alias for [Self::remove_val_at]
    #[deprecated] //GOAT-old-names
    pub fn remove<K: AsRef<[u8]>>(&mut self, path: K) -> Option<V> {
        self.remove_val_at(path)
    }

    /// Returns a reference to the value at the specified `path`, or `None` if no value exists
    pub fn get_val_at<K: AsRef<[u8]>>(&self, path: K) -> Option<&V> {
        let path = path.as_ref();

        //NOTE: Here is the old impl traversing without the zipper.  The zipper version appears to be
        // nearly the same perf.  All averages within 3% in both directions, and the zipper impl being
        // faster as often as the native (non-zipper) version
        // let (node, remaining_key) = traverse_to_leaf(self.root.borrow(), k);
        // node.node_get_val(remaining_key)

        let zipper = self.read_zipper_at_borrowed_path(path);
        zipper.get_val()
    }

    /// Deprecated alias for [Self::get_val_at]
    #[deprecated] //GOAT-old-names
    pub fn get<K: AsRef<[u8]>>(&self, path: K) -> Option<&V> {
        self.get_val_at(path)
    }

    /// Returns a mutable reference to the value at the specified `path` in the `PathMap`, if it exists
    pub fn get_val_mut_at<K: AsRef<[u8]>>(&mut self, path: K) -> Option<&mut V> {
        let path = path.as_ref();
        if path.len() == 0 {
            return self.root_val_mut().as_mut()
        }

        self.ensure_root();
        let root_node = self.root.get_mut().as_mut().unwrap();
        let (node_key, node) = node_along_path_mut(root_node, path, true);
        node.as_tagged_mut().node_into_val_ref_mut(node_key)
    }

    /// Returns a mutable reference to the value at the specified `path`, inserting the result
    /// of `func` if no value exists
    pub fn get_val_or_set_mut_with_at<F, K>(&mut self, path: K, func: F) -> &mut V
    where
    F: FnOnce() -> V,
    K: AsRef<[u8]>,
    {
        let path = path.as_ref();
        if path.len() == 0 {
            if self.root_val().is_some() {
                return self.root_val_mut().as_mut().unwrap()
            }
            *self.root_val_mut() = Some(func());
            return self.root_val_mut().as_mut().unwrap()
        }

        //For setting, it's worth it for us to go through the zipper API, so we don't need
        // to worry about node upgrading, etc.
        self.ensure_root();
        let root_node = self.root.get_mut().as_mut().unwrap();
        let mut temp_z = WriteZipperCore::<'_, '_, V, A>::new_with_node_and_path_in(root_node, None, path, path.len(), 0, self.alloc.clone());

        if !temp_z.is_val() {
            temp_z.set_val(func());
        }
        temp_z.into_value_mut().unwrap()
    }

    /// Returns a mutable reference to the value at the specified `path`, inserting `default`
    /// if no value already exists
    pub fn get_val_or_set_mut_at<K: AsRef<[u8]>>(&mut self, path: K, default: V) -> &mut V {
        self.get_val_or_set_mut_with_at(path, || default)
    }

    /// Returns `true` if the map is empty, otherwise returns `false`
    pub fn is_empty(&self) -> bool {
        (match self.root() {
            Some(root) => root.as_tagged().node_is_empty(),
            None => true
        } && self.root_val().is_none())
    }

    /// Returns the total number of values contained within the map
    ///
    /// WARNING: This is not a cheap method. It may have an order-N cost
    pub fn val_count(&self) -> usize {
        match self.root() {
            Some(root) => val_count_below_root(root.as_tagged()),
            None => 0
        }
    }

    const INVIS_HASH: u128 = 0b00001110010011001111100111000110011110101111001101110110011100001011010011010011001000100111101000001100011111110100001000000111;
    /// Hash the logical `PathMap` and all its values with the provided hash function (which can return INVIS_HASH to ignore values).
    pub fn hash<VHash : Fn(&V) -> u128>(&self, vhash: VHash) -> u128 {
        unsafe {
        self.read_zipper().into_cata_cached(|bm, hs, mv, _| {
            let mut state = [0u8; 48];
            state[0..16].clone_from_slice(gxhash128(slice_from_raw_parts(bm.0.as_ptr() as *const u8, 32).as_ref().unwrap(), 0b0100110001110010000010011111010011100011010000101101111001100110i64).to_le_bytes().as_slice());
            state[16..32].clone_from_slice(gxhash128(slice_from_raw_parts(hs.as_ptr() as *const u8, 16*hs.len()).as_ref().unwrap(), 0b0111010001001011011011011111010110111011111101100110101100010000i64).to_le_bytes().as_slice());
            state[32..].clone_from_slice(mv.map(|v| vhash(v)).unwrap_or(Self::INVIS_HASH).to_le_bytes().as_slice());
            gxhash128(state.as_slice(), 0b0100001010101101111110010110100110000010011000100100100111110111i64)
        })
        }
    }

    /// Returns a new `PathMap` containing the union of the paths in `self` and the paths in `other`
    pub fn join(&self, other: &Self) -> Self where V: Lattice {
        result_into_map(self.pjoin(other), self, other, self.alloc.clone())
    }

    /// Returns a new `PathMap` containing the intersection of the paths in `self` and the paths in `other`
    pub fn meet(&self, other: &Self) -> Self where V: Lattice {
        result_into_map(self.pmeet(other), self, other, self.alloc.clone())
    }

    /// Returns a new `PathMap` where the paths in `self` are restricted by the paths leading to 
    /// values in `other`
    ///
    /// NOTE: if `other` has a root value, this function returns a clone of `self` because other's root
    /// value validates all paths.  If `other` does not have a root value, the returned map won't have
    /// one either.
    pub fn restrict(&self, other: &Self) -> Self {
        if other.root_val().is_some() {
            return self.clone()
        }
        let self_root = self.root();
        let other_root = other.root();
        if self_root.is_none() || other_root.is_none() {
            Self::new_in(self.alloc.clone())
        } else {
            match self_root.unwrap().as_tagged().prestrict_dyn(other_root.unwrap().as_tagged()) {
                AlgebraicResult::Element(new_root) => Self::new_with_root_in(Some(new_root), None, self.alloc.clone()),
                AlgebraicResult::None => Self::new_in(self.alloc.clone()),
                AlgebraicResult::Identity(mask) => {
                    debug_assert_eq!(mask, SELF_IDENT);
                    Self::new_with_root_in(Some(self.root().cloned().unwrap()), None, self.alloc.clone())
                }
            }
        }
    }

    /// Returns a new `PathMap` containing the contents from `self` minus the contents of `other`
    pub fn subtract(&self, other: &Self) -> Self
        where V: DistributiveLattice
    {
        let subtracted_root_val = match self.root_val().psubtract(&other.root_val()) {
            AlgebraicResult::Element(new_val) => new_val,
            AlgebraicResult::Identity(mask) => {
                debug_assert_eq!(mask, SELF_IDENT);
                self.root_val().cloned()
            },
            AlgebraicResult::None => None,
        };

        let subtracted_root_node = match self.root().psubtract(&other.root()) {
            AlgebraicResult::Element(subtracted_node) => subtracted_node,
            AlgebraicResult::Identity(mask) => {
                debug_assert_eq!(mask, SELF_IDENT);
                self.root().cloned()
            },
            AlgebraicResult::None => None,
        };

        Self::new_with_root_in(subtracted_root_node, subtracted_root_val, self.alloc.clone())
    }
}


#[cfg(feature = "old_cursor")]
impl<V: Clone + Send + Sync + Unpin> PathMap<V> {
    /// Returns a [crate::old_cursor::PathMapCursor] to traverse all key-value pairs within the map. This
    /// is more efficient than using [iter](Self::iter), but is not compatible with the [Iterator] trait
    ///
    /// NOTE: This API is deprecated in favor of the [read_zipper](Self::read_zipper) method
    #[deprecated]
    pub fn cursor<'a>(&'a self) -> crate::old_cursor::PathMapCursor<'a, V> {
        crate::old_cursor::PathMapCursor::new(self)
    }

    /// Returns an [crate::old_cursor::AllDenseCursor], which behaves exactly like a [crate::old_cursor::PathMapCursor],
    /// but is only available with the `all_dense_nodes` feature.  This is mainly kept for benchmarking.
    #[deprecated]
    pub fn all_dense_cursor<'a>(&'a self) -> crate::old_cursor::AllDenseCursor<'a, V> {
        crate::old_cursor::AllDenseCursor::new(self)
    }
}

impl<V: Clone + Send + Sync + Unpin, K: AsRef<[u8]>> FromIterator<(K, V)> for PathMap<V> {
    fn from_iter<I: IntoIterator<Item=(K, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (key, val) in iter {
            map.set_val_at(key, val);
        }
        map
    }
}

impl<V: Clone + Send + Sync + Unpin, K: AsRef<[u8]>> From<(K, V)> for PathMap<V> {
    fn from(pair: (K, V)) -> Self {
        let mut map = Self::new();
        map.set_val_at(pair.0, pair.1);
        map
    }
}

impl<V: Clone + Send + Sync + Unpin + 'static, A: Allocator + 'static> std::iter::IntoIterator for PathMap<V, A> {
    type Item = (Vec<u8>, V);
    type IntoIter = OwnedZipperIter<V, A>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_write_zipper(&[]).into_iter()
    }
}

/// Internal function to convert an [AlgebraicResult] (partial lattice result) into a `PathMap`
fn result_into_map<V: Clone + Send + Sync + Unpin, A: Allocator>(result: AlgebraicResult<PathMap<V, A>>, self_map: &PathMap<V, A>, other_map: &PathMap<V, A>, result_region: A) -> PathMap<V, A> {
    match result {
        AlgebraicResult::Element(new_map) => new_map,
        AlgebraicResult::None => PathMap::new_in(result_region),
        AlgebraicResult::Identity(mask) => {
            if mask & SELF_IDENT > 0 {
                self_map.clone()
            } else {
                debug_assert_eq!(mask, COUNTER_IDENT);
                other_map.clone()
            }
        },
    }
}

impl<V: Clone + Lattice + Send + Sync + Unpin, A: Allocator> Lattice for PathMap<V, A> {
    fn pjoin(&self, other: &Self) -> AlgebraicResult<Self> {
        let joined_node = self.root().pjoin(&other.root());
        let joined_root_val = self.root_val().pjoin(&other.root_val());
        joined_node.merge(joined_root_val, |which_arg| {
            match which_arg {
                0 => Some(self.root().cloned()),
                1 => Some(other.root().cloned()),
                _ => unreachable!()
            }
        }, |which_arg| {
            match which_arg {
                0 => Some(self.root_val().cloned()),
                1 => Some(other.root_val().cloned()),
                _ => unreachable!()
            }
        }, |root_node, root_val| {
            AlgebraicResult::Element(Self::new_with_root_in(root_node.flatten(), root_val.flatten(), self.alloc.clone()))
        })
    }
    fn join_into(&mut self, other: Self) -> AlgebraicStatus {
        let (other_root_node, other_root_val) = other.into_root();

        let root_node_status = if let Some(other_root) = other_root_node {
            let (status, result) = self.get_or_init_root_mut().make_mut().join_into_dyn(other_root);
            match result {
                Ok(()) => {},
                Err(replacement) => { *self.get_or_init_root_mut() = replacement; }
            }
            status
        } else {
            if self.is_empty() {
                AlgebraicStatus::None
            } else {
                AlgebraicStatus::Identity
            }
        };

        let root_val_status = self.root_val_mut().join_into(other_root_val);
        root_node_status.merge(root_val_status, true, true)
    }
    fn pmeet(&self, other: &Self) -> AlgebraicResult<Self> {
        let meet_node = self.root().pmeet(&other.root());
        let meet_root_val = self.root_val().pmeet(&other.root_val());
        meet_node.merge(meet_root_val, |which_arg| {
            match which_arg {
                0 => Some(self.root().cloned()),
                1 => Some(other.root().cloned()),
                _ => unreachable!()
            }
        }, |which_arg| {
            match which_arg {
                0 => Some(self.root_val().cloned()),
                1 => Some(other.root_val().cloned()),
                _ => unreachable!()
            }
        }, |root_node, root_val| {
            AlgebraicResult::Element(Self::new_with_root_in(root_node.flatten(), root_val.flatten(), self.alloc.clone()))
        })
    }
}

impl<V: Clone + Send + Sync + Unpin + DistributiveLattice, A: Allocator> DistributiveLattice for PathMap<V, A> {
    fn psubtract(&self, other: &Self) -> AlgebraicResult<Self> {
        let subtract_node = self.root().psubtract(&other.root());
        let subtract_root_val = self.root_val().psubtract(&other.root_val());
        subtract_node.merge(subtract_root_val, |which_arg| {
            match which_arg {
                0 => Some(self.root().cloned()),
                1 => Some(other.root().cloned()),
                _ => unreachable!()
            }
        }, |which_arg| {
            match which_arg {
                0 => Some(self.root_val().cloned()),
                1 => Some(other.root_val().cloned()),
                _ => unreachable!()
            }
        }, |root_node, root_val| {
            AlgebraicResult::Element(Self::new_with_root_in(root_node.flatten(), root_val.flatten(), self.alloc.clone()))
        })
    }
}

impl<V: Clone + Send + Sync + Unpin, A: Allocator> Quantale for PathMap<V, A> {
    fn prestrict(&self, other: &Self) -> AlgebraicResult<Self> {
        if other.root_val().is_some() {
            return AlgebraicResult::Identity(SELF_IDENT)
        }
        match (self.root(), other.root()) {
            (Some(self_root), Some(other_root)) => {
                match self_root.prestrict(other_root) {
                    AlgebraicResult::Element(new_root) => AlgebraicResult::Element(Self::new_with_root_in(Some(new_root), None, self.alloc.clone())),
                    AlgebraicResult::Identity(mask) => {
                        debug_assert_eq!(mask, SELF_IDENT);
                        if self.root_val().is_some() {
                            AlgebraicResult::Element(Self::new_with_root_in(Some(self_root.clone()), None, self.alloc.clone()))
                        } else {
                            AlgebraicResult::Identity(SELF_IDENT)
                        }
                    },
                    AlgebraicResult::None => AlgebraicResult::None,
                }
            },
            _ => AlgebraicResult::None,
        }
    }
}

impl<V: Clone + Send + Sync + Unpin> Default for PathMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::trie_map::*;
    use crate::ring::Lattice;

    #[test]
    fn get_from_map_test() {
        let mut map = PathMap::new();
        //NOW: map contains an empty ListNode

        map.set_val_at("aaaaa", "aaaaa");
        assert_eq!(map.get_val_at("aaaaa").unwrap(), &"aaaaa");
        //NOW: map contains a ListNode with slot_0 filled by a value

        map.set_val_at("bbbbb", "bbbbb");
        assert_eq!(map.get_val_at("bbbbb").unwrap(), &"bbbbb");
        //NOW: map contains a ListNode with slot_0 and slot_1 filled by values

        map.set_val_at("ccccc", "ccccc");
        assert_eq!(map.get_val_at("aaaaa").unwrap(), &"aaaaa");
        assert_eq!(map.get_val_at("bbbbb").unwrap(), &"bbbbb");
        assert_eq!(map.get_val_at("ccccc").unwrap(), &"ccccc");
        //NOW: map contains a DenseByteNode, with 3 separate ListNodes, each containing one value

        map.set_val_at("ddddd", "ddddd");
        assert_eq!(map.get_val_at("ddddd").unwrap(), &"ddddd");
        //NOW: map contains a DenseByteNode, with 4 separate ListNodes, each containing one value

        map.set_val_at("abbbb", "abbbb");
        assert_eq!(map.get_val_at("abbbb").unwrap(), &"abbbb");
        //NOW: Dense("a"..) -> List("aaaa", "bbbb")

        map.set_val_at("aaaab", "aaaab");
        assert_eq!(map.get_val_at("aaaaa").unwrap(), &"aaaaa");
        assert_eq!(map.get_val_at("bbbbb").unwrap(), &"bbbbb");
        assert_eq!(map.get_val_at("abbbb").unwrap(), &"abbbb");
        assert_eq!(map.get_val_at("aaaab").unwrap(), &"aaaab");
        //NOW: Dense("a"..) -> List("aaa", "bbbb") -> List("a", "b")

        map.set_val_at("aaaac", "aaaac");
        assert_eq!(map.get_val_at("aaaaa").unwrap(), &"aaaaa");
        assert_eq!(map.get_val_at("aaaab").unwrap(), &"aaaab");
        assert_eq!(map.get_val_at("aaaac").unwrap(), &"aaaac");
        //NOW: Dense("a"..) -> List("aaa", "bbbb") -> Dense("a", "b", "c")

        map.set_val_at("acaaa", "acaaa");
        assert_eq!(map.get_val_at("aaaaa").unwrap(), &"aaaaa");
        assert_eq!(map.get_val_at("aaaab").unwrap(), &"aaaab");
        assert_eq!(map.get_val_at("aaaac").unwrap(), &"aaaac");
        assert_eq!(map.get_val_at("abbbb").unwrap(), &"abbbb");
        assert_eq!(map.get_val_at("acaaa").unwrap(), &"acaaa");
        //NOW: Dense("a"..) -> Dense("a", "b", "c") a-> List("aa") -> Dense("a", "b", "c")
        //                                          b-> List("bbb")
        //                                          c-> List("aaa")
    }

    #[test]
    fn map_insert_test() {
        let keys = [
            vec![75, 104, 119, 196, 129, 106, 97, 104, 32, 68, 197, 171, 32, 75, 197, 141, 104],
            vec![75, 104, 111, 100, 106, 97, 45, 66, 117, 110, 97, 107],
            vec![75, 104, 111, 100, 122, 104, 97, 45, 68, 111, 107, 117, 107, 104],
            vec![75, 104, 118, 97, 106, 101, 104, 32, 68, 111, 32, 75, 117, 104],
            vec![75, 104, 118, 196, 129, 106, 101, 104, 32, 68, 111, 32, 75, 197, 171, 104],
            vec![75, 104, 119, 97, 106, 97, 32, 68, 111, 32, 75, 111, 104],
            vec![75, 104, 119, 97, 106, 97, 32, 68, 117, 32, 75, 111, 104],
            vec![75, 104, 119, 97, 106, 97, 104, 32, 68, 111, 32, 75, 111, 104],
            vec![75, 104, 119, 97, 106, 97, 104, 32, 68, 117, 32, 75, 111, 104],
            vec![75, 104, 119, 97, 106, 97, 104, 45, 121, 101, 32, 68, 111, 32, 75, 117],
            vec![75, 104, 119, 97, 106, 97, 104, 45, 121, 101, 32, 68, 111, 32, 75, 197, 171],
            vec![75, 104, 119, 196, 129, 106, 97, 32, 68, 111, 32, 75, 111, 104],
            vec![75, 104, 119, 196, 129, 106, 97, 104, 32, 68, 197, 141, 32, 75, 197, 141, 104],
            vec![75, 104, 119, 196, 129, 106, 196, 129, 32, 68, 117, 32, 75, 111, 104],
            vec![107, 104, 119, 97, 106, 104, 32, 100, 119, 32, 107, 119, 104],
            vec![216, 174, 217, 136, 216, 167, 216, 172, 217, 135, 32, 216, 175, 217, 136, 32, 218, 169, 217, 136, 217, 135],
            vec![73, 109, 196, 129, 109, 32, 197, 158, 196, 129, 225, 184, 169, 105, 98],
            vec![69, 109, 97, 109, 32, 83, 97, 104, 101, 98],
            vec![69, 109, 196, 129, 109, 32, 197, 158, 196, 129, 225, 184, 169, 101, 98],
            vec![72, 97, 122, 114, 97, 116],
            vec![73, 109, 97, 109, 32, 83, 97, 104, 101, 98],
            vec![73, 109, 97, 109, 32, 83, 97, 104, 105, 98],
            vec![73, 109, 97, 109, 115, 97, 107, 104, 105, 98],
            vec![73, 109, 196, 129, 109, 32, 83, 196, 129, 225, 186, 150, 101, 98],
            vec![75, 104, 119, 97, 106, 97],
            vec![75, 104, 119, 97, 106, 97, 32, 73, 109, 97, 109, 32, 83, 97, 105, 121, 105, 100]
        ];
        let mut map = PathMap::new();
        for (i, key) in keys.iter().enumerate() {
            map.set_val_at(key, i);
        }
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(map.get_val_at(key), Some(&i));
        }
    }

    #[test]
    fn long_key_map_test() {
        let mut map = PathMap::new();

        map.set_val_at("aaaaaaaaaa01234567890123456789", 30);
        assert_eq!(map.get_val_at("aaaaaaaaaa01234567890123456789").unwrap(), &30);

        map.set_val_at("bbbbbbbbbb012345678901234567891", 31);
        assert_eq!(map.get_val_at("bbbbbbbbbb012345678901234567891").unwrap(), &31);

        map.set_val_at("cccccccccc012345678901234567890123456789", 40);
        assert_eq!(map.get_val_at("cccccccccc012345678901234567890123456789").unwrap(), &40);

        map.set_val_at("dddddddddd01234567890123456789012345678901234", 45);
        assert_eq!(map.get_val_at("dddddddddd01234567890123456789012345678901234").unwrap(), &45);

        map.set_val_at("eeeeeeeeee01234567890123456789012345678901234567890123456789012345678901234567890123456789", 90);
        assert_eq!(map.get_val_at("eeeeeeeeee01234567890123456789012345678901234567890123456789012345678901234567890123456789").unwrap(), &90);
    }

    #[test]
    fn map_contains_path_test() {
        let mut btm = PathMap::new();
        let rs = ["arrow", "bow", "cannon", "roman", "romane", "romanus", "romulus", "rubens", "ruber", "rubicon", "rubicundus", "rom'i"];
        rs.iter().enumerate().for_each(|(i, r)| { btm.set_val_at(r.as_bytes(), i); });

        assert_eq!(btm.contains_path(b"can"), true);
        assert_eq!(btm.contains_path(b"cannon"), true);
        assert_eq!(btm.contains_path(b"cannonade"), false);
        assert_eq!(btm.contains_path(b""), true);
    }

    #[test]
    fn map_get_mut_test() {
        let mut map = PathMap::<usize>::new();

        //Test root value with `get_val_mut_at`
        assert_eq!(map.get_val_mut_at(b""), None);
        assert_eq!(map.set_val_at(b"", 42), None);
        assert_eq!(map.get_val_mut_at(b""), Some(&mut 42));
        *map.get_val_mut_at(b"").unwrap() = 24;
        assert_eq!(map.get_val_mut_at(b""), Some(&mut 24));

        //Test non-root value with `get_val_mut_at`
        const PATH: &[u8] = b"This is a long path to somewhere, hopefully far enough away that we end up creating more than one node";
        assert_eq!(map.get_val_mut_at(PATH), None);
        assert_eq!(map.set_val_at(PATH, 42), None);
        assert_eq!(map.get_val_mut_at(PATH), Some(&mut 42));
        *map.get_val_mut_at(PATH).unwrap() = 24;
        assert_eq!(map.get_val_mut_at(PATH), Some(&mut 24));
    }

    #[test]
    fn map_get_val_mut_or_set_test() {
        let mut map = PathMap::<usize>::new();

        //Test root value
        assert_eq!(map.get_val_or_set_mut_at(b"", 42), &mut 42);
        assert_eq!(map.get_val_mut_at(b""), Some(&mut 42));
        assert_eq!(map.remove_val_at(b""), Some(42));
        *map.get_val_or_set_mut_at(b"", 42) = 24;
        assert_eq!(map.get_val_mut_at(b""), Some(&mut 24));

        //Test non-root value
        const PATH: &[u8] = b"This is a long path to somewhere, hopefully far enough away that we end up creating more than one node";
        assert_eq!(map.get_val_mut_at(PATH), None);
        assert_eq!(map.get_val_or_set_mut_at(PATH, 42), &mut 42);
        assert_eq!(map.get_val_mut_at(PATH), Some(&mut 42));
        assert_eq!(map.remove_val_at(PATH), Some(42));
        *map.get_val_or_set_mut_at(PATH, 42) = 24;
        assert_eq!(map.get_val_mut_at(PATH), Some(&mut 24));
    }

    #[test]
    fn map_remove_test1() {
        let mut map = PathMap::new();
        map.set_val_at("aaaaa", "aaaaa");
        map.set_val_at("bbbbb", "bbbbb");
        map.set_val_at("ccccc", "ccccc");
        map.set_val_at("ddddd", "ddddd");
        map.set_val_at("abbbb", "abbbb");
        map.set_val_at("aaaab", "aaaab");
        map.set_val_at("aaaac", "aaaac");
        map.set_val_at("acaaa", "acaaa");
        assert_eq!(map.val_count(), 8);

        assert_eq!(map.remove_val_at(b"aaaaa"), Some("aaaaa"));
        assert_eq!(map.val_count(), 7);
        assert_eq!(map.remove_val_at(b"acaaa"), Some("acaaa"));
        assert_eq!(map.val_count(), 6);
        assert_eq!(map.remove_val_at(b"cccccnot-a-real-key"), None);
        assert_eq!(map.val_count(), 6);
        assert_eq!(map.remove_val_at(b"aaaac"), Some("aaaac"));
        assert_eq!(map.val_count(), 5);
        assert_eq!(map.remove_val_at(b"aaaab"), Some("aaaab"));
        assert_eq!(map.val_count(), 4);
        assert_eq!(map.remove_val_at(b"abbbb"), Some("abbbb"));
        assert_eq!(map.val_count(), 3);
        assert_eq!(map.remove_val_at(b"ddddd"), Some("ddddd"));
        assert_eq!(map.val_count(), 2);
        assert_eq!(map.remove_val_at(b"ccccc"), Some("ccccc"));
        assert_eq!(map.val_count(), 1);
        assert_eq!(map.remove_val_at(b"bbbbb"), Some("bbbbb"));
        assert_eq!(map.val_count(), 0);
        assert!(map.is_empty());
    }

    #[test]
    fn map_remove_test2() {
        let mut btm = PathMap::from_iter([("abbb", ()), ("b", ()), ("bba", ())].iter().map(|(p, v)| (p.as_bytes(), v)));
        btm.remove_val_at("abbb".as_bytes());
        btm.remove_val_at("a".as_bytes());
    }

    #[test]
    fn map_update_test() {
        let rs = ["arrow", "bow", "cannon", "roman", "romane", "romanus", "romulus", "rubens", "ruber", "rubicon", "rubicundus", "rom'i"];
        let mut btm: PathMap<u64> = rs.into_iter().enumerate().map(|(i, k)| (k, i as u64)).collect();

        let mut zipper = btm.write_zipper_at_path(b"cannon");
        assert_eq!(zipper.get_val_or_set_mut(42), &2);
        drop(zipper);

        let mut zipper = btm.write_zipper_at_path(b"dagger");
        assert_eq!(zipper.get_val_or_set_mut(42), &42);
    }

    #[test]
    fn map_join_test() {
        let mut a = PathMap::<usize>::new();
        let mut b = PathMap::<usize>::new();
        let rs = ["Abbotsford", "Abbottabad", "Abcoude", "Abdul Hakim", "Abdulino", "Abdullahnagar", "Abdurahmoni Jomi", "Abejorral", "Abelardo Luz"];
        for (i, path) in rs.into_iter().enumerate() {
            if i % 2 == 0 {
                a.set_val_at(path, i);
            } else {
                b.set_val_at(path, i);
            }
        }

        let joined = a.join(&b);
        for (path, i) in joined.iter() {
            // println!("{} {}", std::str::from_utf8(&path).unwrap(), i);
            assert_eq!(rs[*i].as_bytes(), &path);
        }
        assert_eq!(joined.val_count(), rs.len());
    }

    #[test]
    fn map_join_into_test() {
        let mut a = PathMap::<usize>::new();
        let mut b = PathMap::<usize>::new();
        let rs = ["Abbotsford", "Abbottabad", "Abcoude", "Abdul Hakim", "Abdulino", "Abdullahnagar", "Abdurahmoni Jomi", "Abejorral", "Abelardo Luz"];
        for (i, path) in rs.into_iter().enumerate() {
            if i % 2 == 0 {
                a.set_val_at(path, i);
            } else {
                b.set_val_at(path, i);
            }
        }

        a.join_into(b);
        for (path, i) in a.iter() {
            // println!("{} {}", std::str::from_utf8(&path).unwrap(), i);
            assert_eq!(rs[*i].as_bytes(), &path);
        }
        assert_eq!(a.val_count(), rs.len());
    }

    #[cfg(feature = "old_cursor")]
    #[test]
    fn cursor_test() {
        let table = ["A", "Bcdef", "Ghij", "Klmnopqrst"];
        let btm: PathMap<usize> = table.iter().enumerate().map(|(n, s)| (s, n)).collect();
        let mut cursor = btm.cursor();
        while let Some((k, v)) = cursor.next() {
            // println!("{}, {v}", std::str::from_utf8(k).unwrap());
            assert_eq!(k, table[*v].as_bytes());
        }
    }

    #[test]
    fn map_root_value_test1() {
        let mut map = PathMap::<usize>::new();

        //Direct-map operations on root value
        assert_eq!(map.get_val_at([]), None);
        assert_eq!(map.set_val_at([], 1), None);
        assert_eq!(map.get_val_at([]), Some(&1));
        assert_eq!(map.remove_val_at([]), Some(1));
        assert_eq!(map.get_val_at([]), None);

        //Through a WriteZipper, created at the root
        let mut z = map.write_zipper();
        assert_eq!(z.val(), None);
        assert_eq!(z.set_val(1), None);
        assert_eq!(z.val(), Some(&1));
        *z.get_val_mut().unwrap() = 2;
        assert_eq!(z.remove_val(), Some(2));
        assert_eq!(z.val(), None);
        drop(z);

        //Through a WriteZipper, created at a zero-length path
        let mut z = map.write_zipper_at_path(&[]);
        assert_eq!(z.val(), None);
        assert_eq!(z.set_val(1), None);
        assert_eq!(z.val(), Some(&1));
        *z.get_val_mut().unwrap() = 2;
        assert_eq!(z.remove_val(), Some(2));
        assert_eq!(z.val(), None);
        drop(z);

        //Through read zippers
        assert_eq!(map.read_zipper().get_val(), None);
        assert_eq!(map.set_val_at([], 1), None);
        assert_eq!(map.read_zipper().get_val(), Some(&1));
        assert_eq!(map.read_zipper_at_borrowed_path(&[]).get_val(), Some(&1));
        assert_eq!(map.read_zipper_at_path([]).get_val(), Some(&1));
        assert_eq!(map.remove_val_at([]), Some(1));
        assert_eq!(map.read_zipper_at_borrowed_path(&[]).get_val(), None);
        assert_eq!(map.read_zipper_at_path([]).get_val(), None);

        //Through ZipperHeads
        let map_head = map.zipper_head();
        let mut z = map_head.write_zipper_at_exclusive_path([]).unwrap();
        assert_eq!(z.val(), None);
        assert_eq!(z.set_val(1), None);
        assert_eq!(z.val(), Some(&1));
        *z.get_val_mut().unwrap() = 2;
        drop(z);
        drop(map_head);
        assert_eq!(map.get_val_at([]), Some(&2));
    }

    /// Tests algebraic ops on maps with root values, but no trie
    #[test]
    fn map_root_value_test2() {
        let mut map_a = PathMap::<()>::new();
        assert_eq!(map_a.get_val_at([]), None);
        assert_eq!(map_a.set_val_at([], ()), None);
        assert_eq!(map_a.get_val_at([]), Some(&()));
        let map_b = PathMap::<()>::new();

        let joined = map_a.join(&map_b);
        assert_eq!(joined.get_val_at([]), Some(&()));

        let mut cloned = map_b.clone();
        cloned.join_into(map_a.clone());
        assert_eq!(cloned.get_val_at([]), Some(&()));

        let meet = map_a.meet(&map_b);
        assert_eq!(meet.get_val_at([]), None);

        let meet = map_a.meet(&map_a);
        assert_eq!(meet.get_val_at([]), Some(&()));

        let subtract = map_a.subtract(&map_b);
        assert_eq!(subtract.get_val_at([]), Some(&()));

        let subtract = map_a.subtract(&map_a);
        assert_eq!(subtract.get_val_at([]), None);

        let subtract = map_a.subtract(&map_a);
        assert_eq!(subtract.get_val_at([]), None);

        let restrict = map_a.restrict(&map_a);
        assert_eq!(restrict.get_val_at([]), Some(&()));

        let restrict = map_a.restrict(&map_b);
        assert_eq!(restrict.get_val_at([]), None);
    }

    /// Tests algebraic ops on maps with root values and a downstream trie
    #[test]
    fn map_root_value_test3() {
        //Both a root val and a trie
        let mut map_a = PathMap::<()>::new();
        assert_eq!(map_a.set_val_at([], ()), None);
        assert_eq!(map_a.set_val_at("AA", ()), None);

        //Trie different from map_a, but no root val
        let mut map_b = PathMap::<()>::new();
        assert_eq!(map_b.set_val_at("BB", ()), None);

        //Trie same as map_a, but no root val
        let mut map_c = PathMap::<()>::new();
        assert_eq!(map_c.set_val_at("AA", ()), None);

        //Root val but no trie
        let mut map_d = PathMap::<()>::new();
        assert_eq!(map_d.set_val_at([], ()), None);

        //pjoin
        let joined_result = map_a.pjoin(&map_b);
        assert!(joined_result.is_element());
        let joined = joined_result.unwrap([&map_a, &map_b]);
        assert_eq!(joined.get_val_at([]), Some(&()));
        assert_eq!(joined.get_val_at("AA"), Some(&()));
        assert_eq!(joined.get_val_at("BB"), Some(&()));

        let joined_result = map_a.pjoin(&map_c);
        assert!(joined_result.is_identity());

        let joined_result = map_a.pjoin(&map_d);
        assert!(joined_result.is_identity());

        //pmeet
        let meet_result = map_a.pmeet(&map_a);
        assert!(meet_result.is_identity());

        let meet_result = map_a.pmeet(&map_b);
        assert!(meet_result.is_none());

        let meet_result = map_a.pmeet(&map_c);
        assert!(meet_result.is_element());
        let meet = meet_result.unwrap([&map_a, &map_c]);
        assert_eq!(meet.get_val_at([]), None);
        assert_eq!(meet.get_val_at("AA"), Some(&()));
        assert_eq!(meet.get_val_at("BB"), None);

        let meet_result = map_a.pmeet(&map_d);
        assert!(meet_result.is_element());
        let meet = meet_result.unwrap([&map_a, &map_d]);
        assert_eq!(meet.get_val_at([]), Some(&()));
        assert_eq!(meet.get_val_at("AA"), None);

        //psubtract
        let subtract_result = map_a.psubtract(&map_a);
        assert!(subtract_result.is_none());

        let subtract_result = map_a.psubtract(&map_b);
        assert!(subtract_result.is_identity());

        let subtract_result = map_a.psubtract(&map_c);
        assert!(subtract_result.is_element());
        let subtract = subtract_result.unwrap([&map_a, &map_c]);
        assert_eq!(subtract.get_val_at([]), Some(&()));
        assert_eq!(subtract.get_val_at("AA"), None);

        let subtract_result = map_a.psubtract(&map_d);
        assert!(subtract_result.is_element());
        let subtract = subtract_result.unwrap([&map_a, &map_d]);
        assert_eq!(subtract.get_val_at([]), None);
        assert_eq!(subtract.get_val_at("AA"), Some(&()));

        //prestrict
        let restrict_result = map_a.prestrict(&map_b);
        assert!(restrict_result.is_none());

        let restrict_result = map_a.prestrict(&map_c);
        assert!(restrict_result.is_element());
        let restrict = restrict_result.unwrap([&map_a, &map_c]);
        assert_eq!(restrict.get_val_at([]), None);
        assert_eq!(restrict.get_val_at("AA"), Some(&()));

        let restrict_result = map_a.prestrict(&map_d);
        assert!(restrict_result.is_identity());
    }

    #[test]
    fn map_root_value_test4() {
        let mut map0 = PathMap::<usize>::new();
        let mut map1 = PathMap::<usize>::new();
        map1.set_val_at([], 0);

        let mut wz = map0.write_zipper();
        wz.graft(&map1.read_zipper());
        drop(wz);

        #[cfg(feature = "graft_root_vals")]
        assert_eq!(map0.get_val_at([]), Some(&0));
        #[cfg(not(feature = "graft_root_vals"))]
        assert_eq!(map0.get_val_at([]), None);
    }

    #[test]
    fn owned_read_zipper_test() {
        let table = ["A", "AB", "Ab", "ABC", "ABc", "ABCD", "B"];
        let map: PathMap<usize> = table.iter().enumerate().map(|(n, s)| (s, n)).collect();
        let mut zipper = map.into_read_zipper(b"AB");

        let expected = [3, 5, 4];
        let mut i = 0;
        while let Some(val) = zipper.to_next_get_val() {
            assert_eq!(*val, expected[i]);
            i += 1;
        }

        let map = zipper.into_map();
        assert_eq!(map.val_count(), 7);
    }
    /// This tests [WriteZipper]s with starting paths inside the map
    #[test]
    fn map_write_zipper_test1() {
        let mut map = PathMap::<isize>::new();
        map.set_val_at(b"start:0000:hello", 0);

        let mut z = map.write_zipper_at_path(b"start:0000:");
        z.descend_to(b"goodbye");
        z.set_val(0);
        drop(z);

        assert_eq!(map.val_count(), 2);
        assert_eq!(map.get_val_at(b"start:0000:hello"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0000:goodbye"), Some(&0));

        let mut map = PathMap::<isize>::new();
        map.set_val_at(b"start:0000:hello", 0);
        map.set_val_at(b"start:0001:hello", 1);
        map.set_val_at(b"start:0002:hello", 2);
        map.set_val_at(b"start:0003:hello", 3);

        let mut z = map.write_zipper_at_path(b"start:0000:");
        z.descend_to(b"goodbye");
        z.set_val(0);
        drop(z);

        let mut z = map.write_zipper_at_path(b"start:0001:");
        z.descend_to(b"goodbye");
        z.set_val(1);
        drop(z);

        let mut z = map.write_zipper_at_path(b"start:0002:");
        z.descend_to(b"goodbye");
        z.set_val(2);
        drop(z);

        let mut z = map.write_zipper_at_path(b"start:0003:");
        z.descend_to(b"goodbye");
        z.set_val(3);
        drop(z);

        assert_eq!(map.val_count(), 8);
        assert_eq!(map.get_val_at(b"start:0000:hello"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0000:goodbye"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0003:hello"), Some(&3));
        assert_eq!(map.get_val_at(b"start:0003:goodbye"), Some(&3));
    }
    /// Identical logic to `map_write_zipper_test2`, but tests [WriteZipperOwned]
    #[test]
    fn map_write_zipper_test2() {
        let mut map = PathMap::<isize>::new();
        map.set_val_at(b"start:0000:hello", 0);

        let mut z = map.into_write_zipper(b"start:0000:");
        z.descend_to(b"goodbye");
        z.set_val(0);
        let map = z.into_map();

        assert_eq!(map.val_count(), 2);
        assert_eq!(map.get_val_at(b"start:0000:hello"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0000:goodbye"), Some(&0));

        let mut map = PathMap::<isize>::new();
        map.set_val_at(b"start:0000:hello", 0);
        map.set_val_at(b"start:0001:hello", 1);
        map.set_val_at(b"start:0002:hello", 2);
        map.set_val_at(b"start:0003:hello", 3);

        let mut z = map.into_write_zipper(b"start:0000:");
        z.descend_to(b"goodbye");
        z.set_val(0);
        let map = z.into_map();

        let mut z = map.into_write_zipper(b"start:0001:");
        z.descend_to(b"goodbye");
        z.set_val(1);
        let map = z.into_map();

        let mut z = map.into_write_zipper(b"start:0002:");
        z.descend_to(b"goodbye");
        z.set_val(2);
        let map = z.into_map();

        let mut z = map.into_write_zipper(b"start:0003:");
        z.descend_to(b"goodbye");
        z.set_val(3);
        let map = z.into_map();

        assert_eq!(map.val_count(), 8);
        assert_eq!(map.get_val_at(b"start:0000:hello"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0000:goodbye"), Some(&0));
        assert_eq!(map.get_val_at(b"start:0003:hello"), Some(&3));
        assert_eq!(map.get_val_at(b"start:0003:goodbye"), Some(&3));
    }
}

//GOAT, Consider refactor of zipper traits.  `WriteZipper` -> `PathWriter`.  Zipper is split into the zipper
// movement traits and a `PathReader` trait.  Then `PathWriter` and `PathReader` can both be implemented on
// the map, and we can get rid of duplicate methods like `graft_map`
