use core::hint::unreachable_unchecked;
use core::mem::{ManuallyDrop, MaybeUninit};
use std::collections::HashMap;

use local_or_heap::LocalOrHeap;

use crate::utils::{BitMask, ByteMask, find_prefix_overlap};
use crate::Allocator;
use crate::trie_node::*;
use crate::ring::*;
use crate::dense_byte_node::{DenseByteNode, ByteNode, CoFree, OrdinaryCoFree, CellCoFree};
use crate::tiny_node::TinyRefNode;

/// A LineListNode stores up to 2 children in a single cache line
#[repr(C)]
pub struct LineListNode<V: Clone + Send + Sync, A: Allocator> {
    #[cfg(feature = "slim_ptrs")]
    refcnt: std::sync::atomic::AtomicU32,
    /// bit 15 = slot_0_used
    /// bit 14 = slot_1_used
    /// bit 13 = slot_0_is_child (child ptr vs value)
    /// bit 12 = slot_1_is_child (child ptr vs value).  If bit 14 is 0, but bit 12 is 1, it means slot_0 consumed all the key space, so nothing can go in slot_1
    /// bits 11 to bit 6 = slot_0_key_len
    /// bit 5 to bit 0 = slot_1_key_len
    key_bytes: [MaybeUninit<u8>; KEY_BYTES_CNT],
    header: u16,
    val_or_child0: ValOrChildUnion<V, A>,
    val_or_child1: ValOrChildUnion<V, A>,
    alloc: A
}
//DISCUSSION: Choosing a KEY_BYTES_CNT size
// The rest of the ListNode is 34 bytes.  So setting KEY_BYTES_CNT=30 means the ListNode is 64 bytes or
// one chache line.  But if we put in into an RcBox, (which adds a 16 byte header) we either need 14 bytes
// to stay within 1 cache line, or 78 to pack into two.
//WARNING the length bits mean I will overflow if I go above 63
#[cfg(feature = "slim_ptrs")]
const KEY_BYTES_CNT: usize = 42;
#[cfg(not(feature = "slim_ptrs"))]
const KEY_BYTES_CNT: usize = 14;

const SLOT_0_USED_MASK: u16 = 1 << 15;
const SLOT_1_USED_MASK: u16 = 1 << 14;
const BOTH_SLOTS_USED_MASK: u16 = SLOT_0_USED_MASK | SLOT_1_USED_MASK;

impl<V: Clone + Send + Sync, A: Allocator> Drop for LineListNode<V, A> {
    fn drop(&mut self) {
        //Discussion: The straightforward recursive implementation hits a stack overflow with, some very
        // long path lengths.  However we don't want to burden the common case with extra work.  The
        // pathological paths are almost entirely non-branching.  Therefore, we will invoke a recursive
        // drop function if the node branches, and an iterative drop if it doesn't

        let slot0_used = self.is_used::<0>();
        let slot1_used = self.is_used::<1>();
        let slot0_child = self.is_child_ptr::<0>();
        let slot1_child = self.is_child_ptr::<1>();

        if  (slot0_used && slot0_child) != (slot1_used && slot1_child)  {
            //If there is exactly one child, do the non-recursive drop
            list_node_iterative_drop(self);
        } else {
            if slot0_used {
                if slot0_child {
                    unsafe{ ManuallyDrop::drop(&mut self.val_or_child0.child) }
                } else {
                    unsafe{ ManuallyDrop::drop(&mut self.val_or_child0.val) }
                }
            }
            if slot1_used {
                if slot1_child {
                    unsafe{ ManuallyDrop::drop(&mut self.val_or_child1.child) }
                } else {
                    unsafe{ ManuallyDrop::drop(&mut self.val_or_child1.val) }
                }
            }
        }
    }
}

#[inline]
fn list_node_iterative_drop<V: Clone + Send + Sync, A: Allocator>(node: &mut LineListNode<V, A>) {
    let mut next_node = list_node_take_child_to_drop(node).unwrap();
    loop {
        if next_node.refcount() > 1 {
            break;
        }
        match next_node.make_mut().as_tagged_mut().into_list() {
            Some(list_node) => {
                match list_node_take_child_to_drop(list_node) {
                    Some(child_node) => {
                        next_node = child_node;
                    }
                    None => break
                }
            },
            None => break
        }
    }
}

#[inline]
fn list_node_take_child_to_drop<V: Clone + Send + Sync, A: Allocator>(node: &mut LineListNode<V, A>) -> Option<TrieNodeODRc<V, A>> {
    let child0 = node.is_used_child_0();
    let child1 = node.is_used_child_1();
    match (child0, child1) {
        (true, false) => {
            if node.is_used::<1>() {
                unsafe{ ManuallyDrop::drop(&mut node.val_or_child1.val) }
            }
            node.header = 0;
            let next_node = unsafe{ ManuallyDrop::take(&mut node.val_or_child0.child) };
            Some(next_node)
        },
        (false, true) => {
            if node.is_used::<0>() {
                unsafe{ ManuallyDrop::drop(&mut node.val_or_child0.val) }
            }
            node.header = 0;
            let next_node = unsafe{ ManuallyDrop::take(&mut node.val_or_child1.child) };
            Some(next_node)
        }
        (true, true) => None, //Since we don't clear the header, the recursive path will end up freeing the downward trie
        (false, false) => None, //Node is already empty of child links; recursive path will drop values
    }
}

impl<V: Clone + Send + Sync, A: Allocator> Clone for LineListNode<V, A> {
    fn clone(&self) -> Self {
        debug_assert!(validate_node(self));
        let val_or_child0 = if self.is_used::<0>() {
            if self.is_child_ptr::<0>() {
                let child = unsafe{ &self.val_or_child0.child }.clone();
                ValOrChildUnion{ child }
            } else {
                let val = unsafe{ &self.val_or_child0.val }.clone();
                ValOrChildUnion{ val }
            }
        } else {
            ValOrChildUnion{ _unused: () }
        };
        let val_or_child1 = if self.is_used::<1>() {
            if self.is_child_ptr::<1>() {
                let child = unsafe{ &self.val_or_child1.child }.clone();
                ValOrChildUnion{ child }
            } else {
                let val = unsafe{ &self.val_or_child1.val }.clone();
                ValOrChildUnion{ val }
            }
        } else {
            ValOrChildUnion{ _unused: () }
        };
        let new_node = Self {
            #[cfg(feature = "slim_ptrs")]
            refcnt: std::sync::atomic::AtomicU32::new(1),
            header: self.header,
            key_bytes: self.key_bytes,
            val_or_child0,
            val_or_child1,
            alloc: self.alloc.clone(),
        };
        debug_assert!(validate_node(&new_node));
        new_node
    }
}

impl<V: Clone + Send + Sync, A: Allocator> core::fmt::Debug for LineListNode<V, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        //Recursively printing a whole tree will get pretty unwieldy.  Should do something
        // like serialization for inspection using standard tools.
        let key_0 = if self.is_used::<0>() {
            let key = unsafe{ self.key_unchecked::<0>() };
            match std::str::from_utf8(key) {
                Ok(str) => str.to_string(),
                Err(_) => format!("{key:?}")
            }
        } else {
            "".to_string()
        };
        let key_1 = if self.is_used::<1>() {
            let key = unsafe{ self.key_unchecked::<1>() };
            match std::str::from_utf8(key) {
                Ok(str) => str.to_string(),
                Err(_) => format!("{key:?}")
            }
        } else {
            "".to_string()
        };
        write!(f,
               "LineListNode (\nslot0: occupied={} is_child={} key={:?}\nslot1: occupied={} is_child={} key={:?})",
               self.is_used::<0>(), self.is_child_ptr::<0>(), key_0,
               self.is_used::<1>(), self.is_child_ptr::<1>(), key_1)
    }
}

impl<V: Clone + Send + Sync, A: Allocator> LineListNode<V, A> {
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self {
            #[cfg(feature = "slim_ptrs")]
            refcnt: std::sync::atomic::AtomicU32::new(1),
            header: 0,
            key_bytes: [MaybeUninit::uninit(); KEY_BYTES_CNT],
            val_or_child0: ValOrChildUnion{ _unused: () },
            val_or_child1: ValOrChildUnion{ _unused: () },
            alloc,
        }
    }
    #[inline]
    pub fn is_used<const SLOT: usize>(&self) -> bool {
        match SLOT {
            0 => self.header & (1 << 15) > 0,
            1 => self.header & (1 << 14) > 0,
            _ => unreachable!()
        }
    }
    /// Returns the number of slots in the node that are in use
    #[inline]
    pub fn used_slot_count(&self) -> usize {
        let masked = self.header & BOTH_SLOTS_USED_MASK;
        match masked {
            0 => 0,
            SLOT_0_USED_MASK => 1,
            BOTH_SLOTS_USED_MASK => 2,
            _ => unreachable!() //Slot1 without Slot0 is invalid
        }
    }
    /// Extracts the flag and length bits assocated with slot_0
    #[inline]
    fn flags_and_len_0(&self) -> usize {
        const LEN_MASK: usize = 0xfc0; //bits 11 to 6, inclusive
        self.header as usize & ((1 << 15) | (1 << 13) | LEN_MASK)
    }
    /// Extracts the flag and length bits assocated with slot_1
    #[inline]
    fn flags_and_len_1(&self) -> usize {
        const LEN_MASK: usize = 0x3f; //bits 5 to 0, inclusive
        (self.header as usize) & ((1 << 14) | (1 << 12) | LEN_MASK)
    }
    /// Constructs a header for slot0
    #[inline]
    fn header0(is_child_ptr: bool, key_len: usize) -> u16 {
        let mask = if is_child_ptr {
            0xa000 | (key_len << 6)
        } else {
            0x8000 | (key_len << 6)
        };
        mask as u16
    }
    /// Constructs a header for slot1
    #[inline]
    fn header1(is_child_ptr: bool, key_len: usize) -> u16 {
        let mask = if is_child_ptr {
            0x5000 | key_len
        } else {
            0x4000 | key_len
        };
        mask as u16
    }
    /// Constructs a header for slot0
    #[inline]
    fn header1_inverse() -> u16 {
        0xafe0
    }
    /// Returns `true` if slot_1 is available to be filled with an entry, otherwise `false`.  The reason
    /// `!is_used_1()` is insufficient is because `slot_1` may be empty but the key storage may be fully
    /// consumed by slot_0's key
    #[inline]
    pub fn is_available_1(&self) -> bool {
        self.header & ((1 << 14) | (1 << 12)) == 0
    }
    #[inline]
    pub fn is_child_ptr<const SLOT: usize>(&self) -> bool {
        match SLOT {
            0 => self.header & (1 << 13) > 0,
            1 => self.header & (1 << 12) > 0,
            _ => unreachable!()
        }
    }
    /// Conceptually identical to is_used_0 && is_child_ptr_0, but with a compound operation
    #[inline]
    pub fn is_used_child_0(&self) -> bool {
        self.header & ((1 << 15) | (1 << 13)) == ((1 << 15) | (1 << 13))
    }
    #[inline]
    pub fn is_used_child_1(&self) -> bool {
        self.header & ((1 << 14) | (1 << 12)) == ((1 << 14) | (1 << 12))
    }
    /// Conceptually identical to is_used_0 && !is_child_ptr_0, but with a compound operation
    #[inline]
    pub fn is_used_value_0(&self) -> bool {
        self.header & ((1 << 15) | (1 << 13)) == (1 << 15)
    }
    #[inline]
    pub fn is_used_value_1(&self) -> bool {
        self.header & ((1 << 14) | (1 << 12)) == (1 << 14)
    }
    #[inline]
    pub fn key_len_0(&self) -> usize {
        const MASK: u16 = 0xfc0; //bits 11 to 6, inclusive
        ((self.header & MASK) >> 6) as usize
    }
    #[inline]
    pub fn key_len_1(&self) -> usize {
        const MASK: u16 = 0x3f; //bits 5 to 0, inclusive
        (self.header & MASK) as usize
    }
    //NOTE: max_key_len_0 == KEY_BYTES_CNT, because LineListNodes are append-only
    // #[inline]
    // pub fn max_key_len_1(&self) -> usize {
    //     KEY_BYTES_CNT - self.key_len_0()
    // }
    #[inline]
    pub(crate) unsafe fn key_unchecked<const SLOT: usize>(&self) -> &[u8] {
        match SLOT {
            0 => core::slice::from_raw_parts(self.key_bytes.as_ptr().cast(), self.key_len_0()),
            1 => {
                let ptr = self.key_bytes.as_ptr().cast::<u8>().add(self.key_len_0());
                core::slice::from_raw_parts(ptr, self.key_len_1())
            },
            _ => unreachable!(),
        }
    }
    #[inline]
    unsafe fn child_in_slot<const SLOT: usize>(&self) -> &TrieNodeODRc<V, A> {
        match SLOT {
            0 => &*self.val_or_child0.child,
            1 => &*self.val_or_child1.child,
            _ => unreachable!()
        }
    }
    #[inline]
    unsafe fn child_in_slot_mut<const SLOT: usize>(&mut self) -> &mut TrieNodeODRc<V, A> {
        match SLOT {
            0 => &mut *self.val_or_child0.child,
            1 => &mut *self.val_or_child1.child,
            _ => unreachable!()
        }
    }
    #[inline]
    unsafe fn val_in_slot<const SLOT: usize>(&self) -> &V {
        match SLOT {
            0 => &**self.val_or_child0.val,
            1 => &**self.val_or_child1.val,
            _ => unreachable!()
        }
    }
    /// Returns a reference to a child or value in the specified slot.  This method is unsafe
    /// because it doesn't check if the slot is occupied and can never return [PayloadRef::None]
    #[inline]
    unsafe fn payload_in_slot<const SLOT: usize>(&self) -> PayloadRef<'_, V, A> {
        match self.is_child_ptr::<SLOT>() {
            true => PayloadRef::Child(unsafe{ self.child_in_slot::<SLOT>() }),
            false => PayloadRef::Val(unsafe{ self.val_in_slot::<SLOT>() })
        }
    }
    fn contains_val(&self, key: &[u8]) -> bool {
        if self.is_used_value_0() {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            if node_key_0 == key {
                return true;
            }
        }
        if self.is_used_value_1() {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            if node_key_1 == key {
                return true;
            }
        }
        false
    }
    fn get_val(&self, key: &[u8]) -> Option<&V> {
        if self.is_used_value_0() {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            if node_key_0 == key {
                let val = unsafe{ self.val_in_slot::<0>() };
                return Some(val);
            }
        }
        if self.is_used_value_1() {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            if node_key_1 == key {
                let val = unsafe{ self.val_in_slot::<1>() };
                return Some(val);
            }
        }
        None
    }
    fn get_val_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        self.get_payload_exact_key_mut::<false>(key)
            .map(|val_or_child| unsafe{ &mut **val_or_child.val })
    }
    fn get_child_mut(&mut self, key: &[u8]) -> Option<(usize, &mut TrieNodeODRc<V, A>)> {
        if self.is_used_child_0() {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            let key_len = self.key_len_0();
            if key.len() >= key_len {
                if node_key_0 == &key[..key_len] {
                    let child = unsafe{ self.child_in_slot_mut::<0>() };
                    return Some((key_len, child))
                }
            }
        }
        if self.is_used_child_1() {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            let key_len = self.key_len_1();
            if key.len() >= key_len {
                if node_key_1 == &key[..key_len] {
                    let child = unsafe{ self.child_in_slot_mut::<1>() };
                    return Some((key_len, child))
                }
            }
        }
        None
    }
    fn get_payload_exact_key_mut<const IS_CHILD: bool>(&mut self, key: &[u8]) -> Option<&mut ValOrChildUnion<V, A>> {
        if self.is_used::<0>() && self.is_child_ptr::<0>() == IS_CHILD {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            if key == node_key_0 {
                return Some(&mut self.val_or_child0)
            }
        }
        if self.is_used::<1>() && self.is_child_ptr::<1>() == IS_CHILD {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            if key == node_key_1 {
                return Some(&mut self.val_or_child1)
            }
        }
        None
    }
    #[inline]
    pub(crate) fn get_both_keys(&self) -> (&[u8], &[u8]) {
        let key0 = if self.is_used::<0>() {
            unsafe{ self.key_unchecked::<0>() }
        } else {
            &[]
        };
        let key1 = if self.is_used::<1>() {
            unsafe{ self.key_unchecked::<1>() }
        } else {
            &[]
        };
        (key0, key1)
    }
    #[cfg(feature = "counters")]
    fn count(&self) -> usize {
        match (self.is_used::<0>(), self.is_used::<1>()) {
            (true, false) => 1,
            (false, false) => 0,
            (true, true) => 2,
            (false, true) => unreachable!(),
        }
    }
}

impl<V: Clone + Send + Sync, A: Allocator> LineListNode<V, A> {
    #[inline]
    unsafe fn set_child_0(&mut self, key: &[u8], child: TrieNodeODRc<V, A>) {
        self.set_payload_0(key, true, ValOrChildUnion{ child: ManuallyDrop::new(child) });
    }
    #[inline]
    unsafe fn set_child_1(&mut self, key: &[u8], child: TrieNodeODRc<V, A>) {
        self.set_payload_1(key, true, ValOrChildUnion{ child: ManuallyDrop::new(child) });
    }
    /// Splits the key in slot_0 at `idx` (exclusive.  ie. the length of the key)
    fn split_0(&mut self, idx: usize) where V: Clone {
        let mut self_payload = ValOrChildUnion{ _unused: () };
        core::mem::swap(&mut self_payload, &mut self.val_or_child0);
        let node_key_0 = unsafe{ self.key_unchecked::<0>() };

        let mut child_node = Self::new_in(self.alloc.clone());
        unsafe{ child_node.set_payload_0(&node_key_0[idx..], self.is_child_ptr::<0>(), self_payload); }

        //Convert slot_0 to a child ptr
        self.val_or_child0 = ValOrChildUnion{ child: ManuallyDrop::new(TrieNodeODRc::new_in(child_node, self.alloc.clone())) };

        //Shift the key for slot_1, if there is one
        let slot_mask_1 = if self.is_used::<1>() {
            let key_len_1 = self.key_len_1();
            unsafe {
                let base_ptr = self.key_bytes.as_mut_ptr().cast::<u8>();
                let src_ptr = base_ptr.add(self.key_len_0());
                let dst_ptr = base_ptr.add(idx);
                core::ptr::copy(src_ptr, dst_ptr, key_len_1);
            }
            self.flags_and_len_1()
        } else {
            0
        };

        //Re-adjust the length and flags
        self.header = (0xa000 | (idx << 6) | slot_mask_1) as u16;
        if idx == KEY_BYTES_CNT {
            self.header |= 1 << 12; //Set the flag state so slot_1 is unavailable
        }
    }
    /// Splits the key in slot_0 at `idx` (exclusive.  ie. the length of the key)
    fn split_1(&mut self, idx: usize) where V: Clone {
        let mut self_payload = ValOrChildUnion{ _unused: () };
        core::mem::swap(&mut self_payload, &mut self.val_or_child1);
        let node_key_1 = unsafe{ self.key_unchecked::<1>() };

        let mut child_node = Self::new_in(self.alloc.clone());
        unsafe{ child_node.set_payload_0(&node_key_1[idx..], self.is_child_ptr::<1>(), self_payload); }

        //Convert slot_0 from to a child ptr
        self.val_or_child1 = ValOrChildUnion{ child: ManuallyDrop::new(TrieNodeODRc::new_in(child_node, self.alloc.clone())) };

        //Re-adjust the length and flags
        let slot_mask_0 = self.flags_and_len_0();
        self.header = (slot_mask_0 | 0x5000 | idx) as u16;
    }
    fn clone_with_updated_payloads(&self, payload_0: Option<ValOrChildUnion<V, A>>, payload_1: Option<ValOrChildUnion<V, A>>) -> Option<Self> {
        match (payload_0, payload_1) {
            (Some(slot0_payload), Some(slot1_payload)) => {
                let mut new_node = Self::new_in(self.alloc.clone());
                let (key0, key1) = self.get_both_keys();
                unsafe{ new_node.set_payload_0(key0, self.is_child_ptr::<0>(), slot0_payload); }
                unsafe{ new_node.set_payload_1(key1, self.is_child_ptr::<1>(), slot1_payload); }
                debug_assert!(validate_node(&new_node));
                Some(new_node)
            },
            (Some(slot0_payload), None) => {
                let mut new_node = Self::new_in(self.alloc.clone());
                let key0 = unsafe{ self.key_unchecked::<0>() };
                unsafe{ new_node.set_payload_0(key0, self.is_child_ptr::<0>(), slot0_payload); }
                debug_assert!(validate_node(&new_node));
                Some(new_node)
            },
            (None, Some(slot1_payload)) => {
                let mut new_node = Self::new_in(self.alloc.clone());
                let key1 = unsafe{ self.key_unchecked::<1>() };
                unsafe{ new_node.set_payload_0(key1, self.is_child_ptr::<1>(), slot1_payload); }
                debug_assert!(validate_node(&new_node));
                Some(new_node)
            },
            (None, None) => None,
        }
    }
    /// Sets the payload and key for slot_0, and ensures slot_1 is empty
    #[inline]
    unsafe fn set_payload_0(&mut self, key: &[u8], is_child_ptr: bool, payload: ValOrChildUnion<V, A>) {
        debug_assert!(key.len() <= KEY_BYTES_CNT);
        core::ptr::copy_nonoverlapping(key.as_ptr(), self.key_bytes.as_mut_ptr().cast(), key.len());
        self.val_or_child0 = payload;
        self.header = Self::header0(is_child_ptr, key.len());
        if key.len() == KEY_BYTES_CNT {
            self.header |= 1 << 12; //Set the flag state so slot_1 is unavailable
        }
    }
    #[inline]
    unsafe fn set_payload_1(&mut self, key: &[u8], is_child_ptr: bool, payload: ValOrChildUnion<V, A>) {
        let key_0_used_cnt = self.key_len_0();
        debug_assert!(key.len() <= KEY_BYTES_CNT - key_0_used_cnt);
        let dst_ptr = self.key_bytes.as_mut_ptr().cast::<u8>().add(key_0_used_cnt);
        core::ptr::copy_nonoverlapping(key.as_ptr(), dst_ptr, key.len());
        self.val_or_child1 = payload;
        self.header |= Self::header1(is_child_ptr, key.len());
    }
    #[inline]
    pub(crate) unsafe fn set_payload_owned<const SLOT: usize>(&mut self, key: &[u8], payload: ValOrChild<V, A>) where V: Clone {
        match SLOT {
            0 => match payload {
                ValOrChild::Child(child) => self.set_payload_0(key, true, ValOrChildUnion{ child: ManuallyDrop::new(child) }),
                ValOrChild::Val(val) => self.set_payload_0(key, false, ValOrChildUnion{ val: ManuallyDrop::new(LocalOrHeap::new(val)) })
            },
            1 => match payload {
                ValOrChild::Child(child) => { self.set_payload_1_no_overflow(key, true, ValOrChildUnion{ child: ManuallyDrop::new(child) }); },
                ValOrChild::Val(val) => { self.set_payload_1_no_overflow(key, false, ValOrChildUnion{ val: ManuallyDrop::new(LocalOrHeap::new(val)) }); }
            },
            _ => unreachable!()
        }
    }
    /// Creates continuation nodes rather than overflowing the key; returns `true` if a continuation node was
    /// created, or false if everything fit within self
    unsafe fn set_payload_0_no_overflow(&mut self, key: &[u8], is_child_ptr: bool, payload: ValOrChildUnion<V, A>) -> bool where V: Clone {
        if key.len() <= KEY_BYTES_CNT {
            //The entire key fits within the node
            unsafe{ self.set_payload_0(key, is_child_ptr, payload); }
            false
        } else {
            //We need to create a number of intermediate nodes to hold the key
            let node_cnt = (key.len()-1) / KEY_BYTES_CNT;
            let child_node_key = &key[(node_cnt * KEY_BYTES_CNT)..];
            debug_assert!(child_node_key.len() > 0);
            debug_assert!(child_node_key.len() <= KEY_BYTES_CNT);
            let mut child_node = Self::new_in(self.alloc.clone());
            child_node.set_payload_0(child_node_key, is_child_ptr, payload);
            let mut next_node = TrieNodeODRc::new_in(child_node, self.alloc.clone());
            for idx in (1..node_cnt).rev() {
                let mut child_node = Self::new_in(self.alloc.clone());
                let child_node_key = &key[(idx*KEY_BYTES_CNT)..((idx+1)*KEY_BYTES_CNT)];
                child_node.set_child_0(child_node_key, next_node);
                next_node = TrieNodeODRc::new_in(child_node, self.alloc.clone());
            }
            self.set_child_0(&key[..KEY_BYTES_CNT], next_node);
            true
        }
    }
    /// Creates continuation nodes rather than overflowing the key; returns `true` if a continuation node was
    /// created, or false if everything fit within self
    unsafe fn set_payload_1_no_overflow(&mut self, key: &[u8], is_child_ptr: bool, payload: ValOrChildUnion<V, A>) -> bool where V: Clone {
        debug_assert!(!self.is_used::<1>());

        //See if we are able to insert any of the key into slot_1
        if self.is_available_1() {
            let remaining_key_bytes = KEY_BYTES_CNT - self.key_len_0();
            if key.len() <= remaining_key_bytes {
                //The entire key fits within the node
                unsafe{ self.set_payload_1(key, is_child_ptr, payload); }
                false
            } else {
                //We need to recursively create a new node to hold the remaining part of the key
                let mut child_node = Self::new_in(self.alloc.clone());
                child_node.set_payload_0_no_overflow(&key[remaining_key_bytes..], is_child_ptr, payload);
                unsafe{ self.set_child_1(&key[..remaining_key_bytes], TrieNodeODRc::new_in(child_node, self.alloc.clone())); }
                true
            }
        } else {
            //If there is a single slot that is occupied but the key consumes the full node, then arbitrarily
            // chop the existing key in half to make room
            self.split_0(KEY_BYTES_CNT / 2);

            //Try again to add the new value to self, now that we've cleared some space
            self.set_payload_1_no_overflow(key, is_child_ptr, payload);
            true
        }
    }
    /// Shifts the contents of slot_0 to slot_1, and puts the supplied payload into slot_0.  Returns `true`
    /// if a continuation node was created, or false if everything fit within self
    unsafe fn set_payload_0_shift_existing(&mut self, key: &[u8], is_child_ptr: bool, payload: ValOrChildUnion<V, A>) -> bool where V: Clone {
        debug_assert!(self.is_used::<0>());
        debug_assert!(!self.is_used::<1>());

        //Make sure some key-space is available in the node
        if self.is_available_1() {
            let old_key_len = self.key_len_0();
            let old_is_child_ptr = self.is_child_ptr::<0>();
            let remaining_key_bytes = KEY_BYTES_CNT - old_key_len;
            let (new_key, new_is_child_ptr, new_payload, created_sub_branch) = if key.len() <= remaining_key_bytes {
                //The entire key fits within the node
                (key, is_child_ptr, payload, false)
            } else {
                //We need to recursively create at least one new node to hold the remaining part of the key
                let mut child_node = Self::new_in(self.alloc.clone());
                child_node.set_payload_0_no_overflow(&key[remaining_key_bytes..], is_child_ptr, payload);
                (&key[..remaining_key_bytes], true, ValOrChildUnion{ child: ManuallyDrop::new(TrieNodeODRc::new_in(child_node, self.alloc.clone())) }, true)
            };
            let new_key_len = new_key.len();
            debug_assert!(new_key_len + old_key_len <= KEY_BYTES_CNT);

            unsafe {
                //Copy the slot_0 key to slot_1, making room for the new key in slot_0
                let base_ptr = self.key_bytes.as_mut_ptr().cast::<u8>();
                let src_ptr: *const u8 = base_ptr;
                let dst_ptr = base_ptr.add(new_key_len);
                core::ptr::copy(src_ptr, dst_ptr, old_key_len);

                //Copy new_key into slot_0
                let src_ptr: *const u8 = new_key.as_ptr();
                let dst_ptr = self.key_bytes.as_mut_ptr().cast();
                core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, new_key_len);
            }

            //Transplant the the slot_0 payload to slot_1
            core::mem::swap(&mut self.val_or_child0, &mut self.val_or_child1);

            //Set the new payload on slot_0
            self.val_or_child0 = new_payload;

            //Construct the new header
            self.header = Self::header0(new_is_child_ptr, new_key_len) | Self::header1(old_is_child_ptr, old_key_len);

            created_sub_branch
        } else {
            //If there is a single slot that is occupied but the key consumes the full node, then arbitrarily
            // chop the existing key in half to make room, and then try again
            self.split_0(KEY_BYTES_CNT / 2);
            self.set_payload_0_shift_existing(key, is_child_ptr, payload);
            true
        }
    }
    /// Takes the contents of SLOT.  If SLOT is 0 then it shifts the contents of slot_1 into slot_0
    fn take_payload<const SLOT: usize>(&mut self) -> Option<ValOrChild<V, A>> {
        if !self.is_used::<SLOT>() {
            return None;
        }
        match SLOT {
            0 => {
                match self.is_child_ptr::<SLOT>() {
                    true => {
                        let child = unsafe{ ManuallyDrop::take(&mut self.val_or_child0.child) };
                        self.shift_1_to_0();
                        Some(ValOrChild::Child(child))
                    },
                    false => {
                        let val = unsafe{ ManuallyDrop::take(&mut self.val_or_child0.val) };
                        self.shift_1_to_0();
                        Some(ValOrChild::Val(LocalOrHeap::into_inner(val)))
                    }
                }
            },
            1 => {
                match self.is_child_ptr::<SLOT>() {
                    true => {
                        let child = unsafe{ ManuallyDrop::take(&mut self.val_or_child1.child) };
                        self.header &= Self::header1_inverse();
                        Some(ValOrChild::Child(child))
                    },
                    false => {
                        let val = unsafe{ ManuallyDrop::take(&mut self.val_or_child1.val) };
                        self.header &= Self::header1_inverse();
                        Some(ValOrChild::Val(LocalOrHeap::into_inner(val)))
                    }
                }
            },
            _ => unreachable!()
        }
    }
    /// Shifts the contents of slot1 into slot0, obliterating the contents of slot0
    fn shift_1_to_0(&mut self) {
        if self.is_used::<1>() {
            self.val_or_child0 = core::mem::take(&mut self.val_or_child1);
            let key_len_1 = self.key_len_1();
            let is_child_1 = self.is_child_ptr::<1>();
            unsafe {
                let base_ptr = self.key_bytes.as_mut_ptr().cast::<u8>();
                let src_ptr = base_ptr.add(self.key_len_0());
                let dst_ptr = base_ptr;
                core::ptr::copy(src_ptr, dst_ptr, key_len_1);
            }
            self.header = Self::header0(is_child_1, key_len_1);
        } else {
            self.header = 0;
        }
    }
    /// Returns the clone of the value or child in the slot
    pub(crate) fn clone_payload<const SLOT: usize>(&self) -> Option<ValOrChild<V, A>> where V: Clone {
        if !self.is_used::<SLOT>() {
            return None;
        }
        match SLOT {
            0 => match self.is_child_ptr::<SLOT>() {
                true => {
                    let child = unsafe{ &*self.val_or_child0.child }.clone();
                    Some(ValOrChild::Child(child))
                },
                false => {
                    let val = unsafe{ &**self.val_or_child0.val }.clone();
                    Some(ValOrChild::Val(val))
                }
            },
            1 => match self.is_child_ptr::<SLOT>() {
                true => {
                    let child = unsafe{ &*self.val_or_child1.child }.clone();
                    Some(ValOrChild::Child(child))
                },
                false => {
                    let val = unsafe{ &**self.val_or_child1.val }.clone();
                    Some(ValOrChild::Val(val))
                }
            },
            _ => unreachable!()
        }
    }
    /// Sets the payload on the node with the specified key, upgrading the node if necessary.
    /// If `is_child_ptr == true`, this method always returns `(None, _)`, if it's false, will return the
    /// replaced value if there was one.
    ///
    /// See [trie_node::TrieNode::node_set_val] for deeper explanation of behavior
    #[inline]
    fn set_payload_abstract<const IS_CHILD: bool>(&mut self, key: &[u8], mut payload: ValOrChildUnion<V, A>) -> Result<(Option<ValOrChild<V, A>>, bool), TrieNodeODRc<V, A>> where V: Clone {

        // A local function to either set a child or a branch on a downstream node
        let set_payload_recursive = |child: &mut dyn TrieNode<V, A>, node_key, payload: ValOrChildUnion<V, A>| {
            if IS_CHILD {
                let onward_link = unsafe{ payload.into_child() };
                return child.node_set_branch(node_key, onward_link).map(|_| (None, true))
            } else {
                let val = unsafe{ payload.into_val() };
                return child.node_set_val(node_key, val).map(|(ret_val, _)| {
                    let ret_val = ret_val.map(|val| ValOrChild::Val(val));
                    (ret_val, true)
                })
            }
        };

        //If we already have a payload at the key, then replace it
        if let Some(node_payload) = self.get_payload_exact_key_mut::<IS_CHILD>(key) {
            core::mem::swap(&mut payload, node_payload);
            return Ok((Some(ValOrChild::from_union::<IS_CHILD>(payload)), false));
        }

        //If this node is empty, insert the new key-payload into slot_0
        if !self.is_used::<0>() {
            let created_subnode = unsafe{ self.set_payload_0_no_overflow(key, IS_CHILD, payload) };
            return Ok((None, created_subnode))
        }

        //If the key has overlap with slot_0, split the key, and add the payload to the child
        let node_key_0 = unsafe{ self.key_unchecked::<0>() };
        let mut overlap = find_prefix_overlap(key, node_key_0);
        if overlap > 0 {
            //See if we should totally replace the existing downstream branch
            if IS_CHILD && self.is_child_ptr::<0>() && overlap == key.len() {
                let _ = self.take_payload::<0>();
                return self.set_payload_abstract::<IS_CHILD>(key, payload)
            }
            //Otherwise add in a new branch
            if overlap == node_key_0.len() || overlap == key.len() {
                overlap -= 1;
            }
            if overlap > 0 {
                self.split_0(overlap);
                let child = unsafe{ self.child_in_slot_mut::<0>() }.make_mut();
                return set_payload_recursive(child, &key[overlap..], payload)
            }
        }

        //See if we are able to fill slot_1, either by inserting directly or shifting from slot_0
        if !self.is_used::<1>() {
            let created_subnode = if should_swap_keys(node_key_0, key) {
                unsafe{ self.set_payload_0_shift_existing(key, IS_CHILD, payload) }
            } else {
                unsafe{ self.set_payload_1_no_overflow(key, IS_CHILD, payload) }
            };
            return Ok((None, created_subnode))
        }

        //If the key has overlap with slot_1, split that key
        let node_key_1 = unsafe{ self.key_unchecked::<1>() };
        let mut overlap = find_prefix_overlap(key, node_key_1);
        if overlap > 0 {
            //See if we should totally replace the existing downstream branch
            if IS_CHILD && self.is_child_ptr::<1>() && overlap == key.len() {
                let _ = self.take_payload::<1>();
                return self.set_payload_abstract::<IS_CHILD>(key, payload)
            }
            //Otherwise add in a new branch
            if overlap == node_key_1.len() || overlap == key.len() {
                overlap -= 1;
            }
            if overlap > 0 {
                self.split_1(overlap);
                let child = unsafe{ self.child_in_slot_mut::<1>() }.make_mut();
                return set_payload_recursive(child, &key[overlap..], payload)
            }
        }

        //We couldn't store the value in either of the slots, so upgrade the node
        //=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=
        let mut replacement_node = self.convert_to_dense::<OrdinaryCoFree<V, A>>(3);
        let dense_node = replacement_node.make_mut().as_tagged_mut().into_dense().unwrap();

        //Add the new key-value pair to the new DenseByteNode
        if key.len() > 1 {
            let mut child_node = Self::new_in(self.alloc.clone());
            set_payload_recursive(&mut child_node, &key[1..], payload).unwrap_or_else(|_| panic!());
            dense_node.set_child(key[0], TrieNodeODRc::new_in(child_node, self.alloc.clone()));
        } else {
            if IS_CHILD {
                dense_node.set_child(key[0], unsafe{ payload.into_child() });
            } else {
                dense_node.set_val(key[0], unsafe{ payload.into_val() });
            }
        }

        Err(replacement_node)
    }

    /// Ensures that a node is valid by combining an illegal shared prefix between the keys if there is one
    /// This is currently used by drop_head, because dropping a disjoint prefix may cause downstream paths
    /// to collide, and thus require merging
    fn factor_prefix(&mut self) where V: Clone + Lattice {
        let (key0, key1) = self.get_both_keys();
        let overlap = find_prefix_overlap(key0, key1);
        //Overlap of 1 is legal if and only if ONE OF the following two conditions are true:
        // A: slot0 contains a value
        // B: both slots have a length of 1, and one is a value
        let legal_overlap = overlap == 1 && (
            !self.is_child_ptr::<0>() ||
            (!self.is_child_ptr::<1>() && key0.len()==1 && key1.len()==1 ));

        //If the overlap is illegal, split the prefix
        if overlap > 0 && !legal_overlap {
            match merge_guts::<V, A, 0, 1>(overlap, key0, self, key1, self) {
                AlgebraicResult::Element((shared_key, merged_payload)) => {
                    let mut new_node = Self::new_in(self.alloc.clone());
                    unsafe{ new_node.set_payload_owned::<0>(shared_key, merged_payload) };
                    *self = new_node;
                },
                AlgebraicResult::Identity(mask) => {
                    debug_assert!(mask & SELF_IDENT > 0);
                    let mut new_node = Self::new_in(self.alloc.clone());
                    unsafe{ new_node.set_payload_owned::<0>(key0, self.clone_payload::<0>().unwrap()) };
                    *self = new_node;
                },
                AlgebraicResult::None => {}
            }
        }
    }

    /// Converts the node to a ByteNode, transplanting the contents and leaving `self` empty
    pub(crate) fn convert_to_dense<Cf: CoFree<V=V, A=A>>(&mut self, capacity: usize) -> TrieNodeODRc<V, A>
        where ByteNode<Cf, A>: TrieNodeDowncast<V, A>
    {
        let mut replacement_node = ByteNode::<Cf, A>::with_capacity_in(capacity, self.alloc.clone());

        //1. Transplant the key / value from slot_1 to the new node
        if self.is_used::<0>() {
            let mut slot_0_payload = ValOrChildUnion{ _unused: () };
            core::mem::swap(&mut slot_0_payload, &mut self.val_or_child0);
            let key_0 = unsafe{ self.key_unchecked::<0>() };
            //DenseByteNodes hold one byte keys, so if the key is more than 1 byte we need to
            // make an intermediate node to hold the rest of the key
            if key_0.len() > 1 {
                let mut child_node = Self::new_in(self.alloc.clone());
                unsafe{ child_node.set_payload_0(&key_0[1..], self.is_child_ptr::<0>(), slot_0_payload); }
                replacement_node.set_child(key_0[0], TrieNodeODRc::new_in(child_node, self.alloc.clone()));
            } else {
                if self.is_child_ptr::<0>() {
                    let child_node = unsafe{ ManuallyDrop::into_inner(slot_0_payload.child) };
                    replacement_node.set_child(key_0[0], child_node);
                } else {
                    let val_0 = unsafe{ ManuallyDrop::into_inner(slot_0_payload.val) };
                    replacement_node.set_val(key_0[0], LocalOrHeap::into_inner(val_0));
                }
            }
        }

        //2. Transplant the key / value from slot_1 to the new node
        if self.is_used::<1>() {
            let mut slot_1_payload = ValOrChildUnion{ _unused: () };
            core::mem::swap(&mut slot_1_payload, &mut self.val_or_child1);
            let key_1 = unsafe{ self.key_unchecked::<1>() };
            if key_1.len() > 1 {
                let mut child_node = Self::new_in(self.alloc.clone());
                unsafe{ child_node.set_payload_0(&key_1[1..], self.is_child_ptr::<1>(), slot_1_payload); }
                replacement_node.set_child(key_1[0], TrieNodeODRc::new_in(child_node, self.alloc.clone()));
            } else {
                if self.is_child_ptr::<1>() {
                    let child_node = unsafe{ ManuallyDrop::into_inner(slot_1_payload.child) };
                    replacement_node.set_child(key_1[0], child_node);
                } else {
                    let val_1 = unsafe{ ManuallyDrop::into_inner(slot_1_payload.val) };
                    replacement_node.set_val(key_1[0], LocalOrHeap::into_inner(val_1));
                }
            }
        }

        //4. Clear self.header, so we don't double-free anything when this old node gets dropped
        self.header = 0;

        TrieNodeODRc::new_in(replacement_node, self.alloc.clone())
    }

    //GOAT Dead code, in favor of `pmeet_generic`  Currently it's not deleted because we may refer back to it
    // but it's slated for deletion soon.
    //
    // /// Internal method to meet the contents of `SLOT` with the contents of the `other` node
    // #[inline]
    // fn meet_slot_contents<const SLOT: usize>(&self, other: &dyn TrieNode<V>) -> FatAlgebraicResult<ValOrChild<V>> where V: Clone + Lattice {
    //     if self.is_used::<SLOT>() {
    //         let path = unsafe{ self.key_unchecked::<SLOT>() };
    //         if let Some((onward_key, onward_node)) = follow_path(other, path) {
    //             if self.is_child_ptr::<SLOT>() {
    //                 let self_onward_link = unsafe{ self.child_in_slot::<SLOT>() };
    //                 let (meet_result, other_node) = if onward_key.len() == 0 {
    //                     //GOAT!!!!, if onward_node were a TrieNodeODRc, we would avoid having to make a deep copy here, and more importantly
    //                     // breaking the sharing!!
    //                     (self_onward_link.borrow().pmeet_dyn(onward_node), Some(onward_node.clone_self()))
    //                 } else {
    //                     match onward_node.get_node_at_key(onward_key).into_option() {
    //                         Some(other_onward_node) => {
    //                             (self_onward_link.borrow().pmeet_dyn(other_onward_node.borrow()), Some(other_onward_node))
    //                         },
    //                         None => {
    //                             if other.node_is_empty() {
    //                                 return FatAlgebraicResult::new(COUNTER_IDENT, None)
    //                             } else {
    //                                 return FatAlgebraicResult::new(0, None)
    //                             }
    //                         }
    //                     }
    //                 };
    //                 match meet_result {
    //                     AlgebraicResult::None => FatAlgebraicResult::none(),
    //                     AlgebraicResult::Element(node) => FatAlgebraicResult::element(ValOrChild::Child(node)),
    //                     AlgebraicResult::Identity(mask) => {
    //                         if mask & SELF_IDENT > 0 {
    //                             FatAlgebraicResult::new(mask, Some(ValOrChild::Child(self_onward_link.clone())))
    //                         } else {
    //                             debug_assert_eq!(mask, COUNTER_IDENT);
    //                             debug_assert!(other_node.is_some());
    //                             FatAlgebraicResult::new(mask, other_node.map(|node| ValOrChild::Child(node)))
    //                         }
    //                     }
    //                 }
    //             } else {
    //                 let self_val = unsafe{ self.val_in_slot::<SLOT>() };
    //                 if let Some(other_val) = onward_node.node_get_val(onward_key) {
    //                     match self_val.pmeet(other_val) {
    //                         AlgebraicResult::None => FatAlgebraicResult::none(),
    //                         AlgebraicResult::Element(val) => FatAlgebraicResult::element(ValOrChild::Val(val)),
    //                         AlgebraicResult::Identity(mask) => {
    //                             if mask & SELF_IDENT > 0 {
    //                                 FatAlgebraicResult::new(mask, Some(ValOrChild::Val(self_val.clone())))
    //                             } else {
    //                                 debug_assert_eq!(mask, COUNTER_IDENT);
    //                                 FatAlgebraicResult::new(mask, Some(ValOrChild::Val(other_val.clone())))
    //                             }
    //                         }
    //                     }
    //                 } else {
    //                     if other.node_is_empty() {
    //                         FatAlgebraicResult::new(COUNTER_IDENT, None)
    //                     } else {
    //                         FatAlgebraicResult::new(0, None)
    //                     }
    //                 }
    //             }
    //         } else {
    //             if other.node_is_empty() {
    //                 FatAlgebraicResult::new(COUNTER_IDENT, None)
    //             } else {
    //                 FatAlgebraicResult::new(0, None)
    //             }
    //         }
    //     } else {
    //         FatAlgebraicResult::new(SELF_IDENT, None)
    //     }
    // }

    /// Internal method to subtract the contents of `SLOT` with the contents of the `other` node
    fn subtract_from_slot_contents<const SLOT: usize>(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<ValOrChildUnion<V, A>> where V: Clone + DistributiveLattice {
        if !self.is_used::<SLOT>() {
            return AlgebraicResult::None
        }
        let path = unsafe{ self.key_unchecked::<SLOT>() };
        if let Some((onward_key, onward_node)) = follow_path(other, path) {
            if self.is_child_ptr::<SLOT>() {
                let self_onward_link = unsafe{ self.child_in_slot::<SLOT>() };
                let difference = if onward_key.len() == 0 {
                    self_onward_link.borrow().psubtract_dyn(onward_node)
                } else {
                    match onward_node.get_node_at_key(onward_key).into_option() {
                        Some(other_onward_node) => self_onward_link.borrow().psubtract_dyn(other_onward_node.borrow()),
                        None => return AlgebraicResult::Identity(SELF_IDENT)
                    }
                };
                debug_assert!(difference.as_ref().map(|node| node.borrow().as_tagged().as_list().map(|node| validate_node(node)).unwrap_or(true)).unwrap_or([true, true], true));
                difference.map(|node| ValOrChildUnion::from(node))
            } else {
                debug_assert!(onward_key.len() > 0);
                let self_val = unsafe{ self.val_in_slot::<SLOT>() };
                let other_val = onward_node.node_get_val(onward_key).unwrap();
                self_val.psubtract(other_val).map(|val| ValOrChildUnion::from(val))
            }
        } else {
            //We subtracted nothing from the slot, so the source should be referenced, unmodified
            AlgebraicResult::Identity(SELF_IDENT)
        }
    }
    /// Internal method to restrict the contents of `SLOT` with the contents of the `other` node
    fn restrict_slot_contents<const SLOT: usize>(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<ValOrChildUnion<V, A>> where V: Clone {
        if self.is_used::<SLOT>() {
            let path = unsafe{ self.key_unchecked::<SLOT>() };
            let (found_val, onward) = follow_path_to_value(other, path);
            if found_val {
                return AlgebraicResult::Identity(SELF_IDENT);
            }
            if let Some((onward_key, onward_node)) = onward {
                if self.is_child_ptr::<SLOT>() {
                    let self_onward_link = unsafe{ self.child_in_slot::<SLOT>() };
                    let restricted_node_result = if onward_key.len() == 0 {
                        self_onward_link.borrow().prestrict_dyn(onward_node)
                    } else {
                        let other_onward_node = onward_node.get_node_at_key(onward_key);
                        self_onward_link.borrow().prestrict_dyn(other_onward_node.borrow())
                    };
                    restricted_node_result.map(|node| ValOrChildUnion::from(node))
                } else {
                    AlgebraicResult::None
                }
            } else {
                AlgebraicResult::None
            }
        } else {
            AlgebraicResult::None
        }
    }

    /// Internal method to combine the result from separate **Non-Commutative** operations on individual slots into
    /// an AlgebraicResult for the whole node
    #[inline]
    fn combine_slot_results_into_node_result(&self, slot0_result: AlgebraicResult<ValOrChildUnion<V, A>>, slot1_result: AlgebraicResult<ValOrChildUnion<V, A>>) -> AlgebraicResult<TrieNodeODRc<V, A>> {
        let (slot0_payload, slot1_payload) = match (slot0_result, slot1_result) {
            (AlgebraicResult::Identity(mask0), AlgebraicResult::Identity(mask1)) => {
                debug_assert_eq!(mask0, SELF_IDENT);
                debug_assert_eq!(mask1, SELF_IDENT);
                return AlgebraicResult::Identity(SELF_IDENT)
            },
            (AlgebraicResult::None, AlgebraicResult::None) => return AlgebraicResult::None,
            (AlgebraicResult::Identity(mask), AlgebraicResult::None) => {
                if !self.is_used::<1>() {
                    debug_assert_eq!(mask, SELF_IDENT);
                    return AlgebraicResult::Identity(SELF_IDENT)
                } else {
                    let slot0_payload = self.clone_payload::<0>().map(|payload| payload.into());
                    (slot0_payload, None)
                }
            },
            // NOTE: There is no need to special-case the (AlgebraicResult::None, AlgebraicResult::Identity)
            // case, because if slot1 can't have contents if slot0 is empty, therefore we know that if
            // slot0 is None, and we didn't hit the (None, None) case above, then the case below is the
            // correct case to handle this situation
            (e0, e1) => {
                let slot0_payload = e0.map_into_option(|arg_idx| {
                    debug_assert_eq!(arg_idx, 0);
                    self.clone_payload::<0>().map(|payload| payload.into())
                });
                let slot1_payload = e1.map_into_option(|arg_idx| {
                    debug_assert_eq!(arg_idx, 0);
                    self.clone_payload::<1>().map(|payload| payload.into())
                });
                (slot0_payload, slot1_payload)
            }
        };
        debug_assert!(slot0_payload.is_some() || slot1_payload.is_some());
        let new_node = self.clone_with_updated_payloads(slot0_payload, slot1_payload).unwrap();
        AlgebraicResult::Element(TrieNodeODRc::new_in(new_node, self.alloc.clone()))
    }
}


/// Returns the part of `src_key` that remains after excluding the first `key_len` bytes
#[inline]
fn remaining_key(src_key: &[u8], key_len: usize) -> &[u8] {
    if src_key.len() > key_len {
        &src_key[key_len..]
    } else {
        &[]
    }
}

/// Returns `true` if key1 belongs in slot_0 and key0 should go in slot_1, to preserve legal ordering
#[inline]
fn should_swap_keys(key0: &[u8], key1: &[u8]) -> bool {
    debug_assert!(key0.len() > 0);
    debug_assert!(key1.len() > 0);

    if key0[0] > key1[0] {
        return true;
    }
    if key0[0] == key1[0] && key0.len() > key1.len() {
        return true;
    }
    false
}

/// Attempts to merge a specific slot in a ListNode with a specific slot in another ListNode.  Returns the merged
/// (key, payload) pair if a merge was possible, otherwise None
fn try_merge<'a, V: Clone + Lattice + Send + Sync, A: Allocator, const ASLOT: usize, const BSLOT: usize>(a_key: &'a[u8], a: &LineListNode<V, A>, b_key: &'a[u8], b: &LineListNode<V, A>) -> AlgebraicResult<(&'a[u8], ValOrChild<V, A>)> {
    //Are there are any common paths between the nodes?
    let overlap = find_prefix_overlap(a_key, b_key);
    if overlap > 0 {
        merge_guts::<V, A, ASLOT, BSLOT>(overlap, a_key, a, b_key, b)
    } else {
        AlgebraicResult::None //No overlap between keys
    }
}

/// The part of `try_merge` that we probably shouldn't inline
fn merge_guts<'a, V: Clone + Lattice + Send + Sync, A: Allocator, const ASLOT: usize, const BSLOT: usize>(mut overlap: usize, a_key: &'a[u8], a: &LineListNode<V, A>, b_key: &'a[u8], b: &LineListNode<V, A>) -> AlgebraicResult<(&'a[u8], ValOrChild<V, A>)> {
    debug_assert!(overlap > 0);
    let a_key_len = a_key.len();
    let b_key_len = b_key.len();

    // Algorithm Overview:
    // In order to create valid new node, we must adhere to a number of constraints, which means a handful
    // of cases to handle
    // - if identical keys, and both are a child or both are a value, then join the key or value and return
    //      a 1-entry node
    // - if the shorter remaining key is a child, then chop the longer key, make a new node containing just
    //      the long value, and join the new node with the other child
    // - otherwise see if we can build a node at the end of the common prefix, with the shorter val/child
    //      in slot0 and longer val/child in slot1

    //Check for identical keys
    if overlap == a_key_len && overlap == b_key_len {
        match (a.is_child_ptr::<ASLOT>(), b.is_child_ptr::<BSLOT>()) {
            (true, true) => { //both are child nodes, so join them
                let a_child = unsafe{ a.child_in_slot::<ASLOT>() };
                let b_child = unsafe{ b.child_in_slot::<BSLOT>() };
                return a_child.pjoin(b_child).map(|new_child| (a_key, ValOrChild::Child(new_child)))
            },
            (false, false) => { //both are values, so join them
                let a_val = unsafe{ a.val_in_slot::<ASLOT>() };
                let b_val = unsafe{ b.val_in_slot::<BSLOT>() };
                return a_val.pjoin(b_val).map(|new_val| (a_key, ValOrChild::Val(new_val)))
            },
            _ => {}
        }
    }

    //We're never allowed to have an onward child key that is shorter than another key, so if that's
    // the case we need to split the longer key, and try to join the resulting nodes
    if b_key_len == overlap && b.is_child_ptr::<BSLOT>() && a_key_len > overlap {
        let a_payload = a.clone_payload::<ASLOT>().unwrap();
        let b_child = unsafe{ b.child_in_slot::<BSLOT>() };
        let mut intermediate_node = LineListNode::new_in(a.alloc.clone());
        unsafe{ intermediate_node.set_payload_owned::<0>(&a_key[overlap..], a_payload); }
        debug_assert!(validate_node(&intermediate_node));
        let intermediate_node = TrieNodeODRc::new_in(intermediate_node, a.alloc.clone());
        let joined = b_child.pjoin(&intermediate_node).unwrap_or_else(|which_arg| {
            match which_arg {
                0 => b_child.clone(),
                1 => intermediate_node,
                _ => unreachable!()
            }
        }, || panic!());
        return AlgebraicResult::Element((&a_key[0..overlap], ValOrChild::Child(joined)))
    }
    if a_key_len == overlap && a.is_child_ptr::<ASLOT>() && b_key_len > overlap {
        let a_child = unsafe{ a.child_in_slot::<ASLOT>() };
        let b_payload = b.clone_payload::<BSLOT>().unwrap();
        let mut intermediate_node = LineListNode::new_in(a.alloc.clone());
        unsafe{ intermediate_node.set_payload_owned::<0>(&b_key[overlap..], b_payload); }
        debug_assert!(validate_node(&intermediate_node));
        let intermediate_node = TrieNodeODRc::new_in(intermediate_node, a.alloc.clone());
        let joined = a_child.pjoin(&intermediate_node).unwrap_or_else(|which_arg| {
            match which_arg {
                0 => a_child.clone(),
                1 => intermediate_node,
                _ => unreachable!()
            }
        }, || panic!());
        return AlgebraicResult::Element((&a_key[0..overlap], ValOrChild::Child(joined)))
    }

    //If we have overlapping initial bytes that can be joined together, make a new prefix node
    if overlap == a_key_len || overlap == b_key_len {
        overlap -= 1;
    }
    if overlap > 0 {
        let mut new_node = LineListNode::new_in(a.alloc.clone());
        let a_payload = a.clone_payload::<ASLOT>().unwrap();
        let b_payload = b.clone_payload::<BSLOT>().unwrap();

        // Put the keys into the right slots based on their first bytes or lengths if the initial bytes are equal
        let new_a_key = &a_key[overlap..];
        let new_b_key = &b_key[overlap..];
        if new_a_key[0] == new_b_key[0] {
            if a_key_len < b_key_len {
                unsafe{ new_node.set_payload_owned::<0>(new_a_key, a_payload); }
                unsafe{ new_node.set_payload_owned::<1>(new_b_key, b_payload); }
            } else {
                unsafe{ new_node.set_payload_owned::<0>(new_b_key, b_payload); }
                unsafe{ new_node.set_payload_owned::<1>(new_a_key, a_payload); }
            }
        } else {
            if new_a_key[0] < new_b_key[0] {
                unsafe{ new_node.set_payload_owned::<0>(new_a_key, a_payload); }
                unsafe{ new_node.set_payload_owned::<1>(new_b_key, b_payload); }
            } else {
                unsafe{ new_node.set_payload_owned::<0>(new_b_key, b_payload); }
                unsafe{ new_node.set_payload_owned::<1>(new_a_key, a_payload); }
            }
        }
        debug_assert!(validate_node(&new_node));
        AlgebraicResult::Element((&a_key[..overlap], ValOrChild::Child(TrieNodeODRc::new_in(new_node, a.alloc.clone()))))
    } else {
        AlgebraicResult::None
    }
}

fn merge_list_nodes<V: Clone + Send + Sync + Lattice, A: Allocator>(a: &LineListNode<V, A>, b: &LineListNode<V, A>) -> Result<AlgebraicResult<LineListNode<V, A>>, AlgebraicResult<DenseByteNode<V, A>>> {
    debug_assert!(validate_node(a));
    debug_assert!(validate_node(b));

    let (self_key0, self_key1) = a.get_both_keys();
    let (other_key0, other_key1) = b.get_both_keys();
    let mut entries: [MaybeUninit<(&[u8], ValOrChild<V, A>)>; 4] = [MaybeUninit::uninit(), MaybeUninit::uninit(), MaybeUninit::uninit(), MaybeUninit::uninit()];
    let mut entry_cnt = 0;
    let mut used: [bool; 4] = [false; 4]; //[self_0, self_1, other_0, other_1]
    let mut identity_masks: [u64; 4] = [0; 4];

    // Try each pairing in self and other, to see if there is a key-join that can happen
    // We can assume two keys in the same node can't merge, because they would have already been merged,
    // and therefore we can also assume that if a key can be merged with one key of a node it can't be
    // merged with the other
    match try_merge::<V, A, 0, 0>(self_key0, a, other_key0, b) {
        AlgebraicResult::Element(joined) => {
            entries[entry_cnt] = MaybeUninit::new(joined);
            entry_cnt += 1;
            used[0] = true;
            used[2] = true;
        },
        AlgebraicResult::Identity(mask) => {
            if mask & SELF_IDENT > 0 {
                entries[entry_cnt] = MaybeUninit::new((self_key0, a.clone_payload::<0>().unwrap()));
            } else {
                debug_assert!(mask & COUNTER_IDENT > 0);
                entries[entry_cnt] = MaybeUninit::new((other_key0, b.clone_payload::<0>().unwrap()));
            }
            identity_masks[entry_cnt] = mask;
            entry_cnt += 1;
            used[0] = true;
            used[2] = true;
        },
        AlgebraicResult::None => { }
    }
    match try_merge::<V, A, 0, 1>(self_key0, a, other_key1, b) {
        AlgebraicResult::Element(joined) => {
            entries[entry_cnt] = MaybeUninit::new(joined);
            entry_cnt += 1;
            debug_assert!(used[0] == false); //If we create multiple joined entries from the same source, it's a bug somewhere
            used[0] = true;
            used[3] = true;
        },
        AlgebraicResult::Identity(mask) => {
            if mask & SELF_IDENT > 0 {
                entries[entry_cnt] = MaybeUninit::new((self_key0, a.clone_payload::<0>().unwrap()));
            } else {
                debug_assert!(mask & COUNTER_IDENT > 0);
                entries[entry_cnt] = MaybeUninit::new((other_key1, b.clone_payload::<1>().unwrap()));
            }
            identity_masks[entry_cnt] = mask;
            entry_cnt += 1;
            debug_assert!(used[0] == false); //See above
            used[0] = true;
            used[3] = true;
        },
        AlgebraicResult::None => {}
    }
    match try_merge::<V, A, 1, 0>(self_key1, a, other_key0, b) {
        AlgebraicResult::Element(joined) => {
            entries[entry_cnt] = MaybeUninit::new(joined);
            entry_cnt += 1;
            debug_assert!(used[2] == false); //See above
            used[1] = true;
            used[2] = true;
        },
        AlgebraicResult::Identity(mask) => {
            if mask & SELF_IDENT > 0 {
                entries[entry_cnt] = MaybeUninit::new((self_key1, a.clone_payload::<1>().unwrap()));
            } else {
                debug_assert!(mask & COUNTER_IDENT > 0);
                entries[entry_cnt] = MaybeUninit::new((other_key0, b.clone_payload::<0>().unwrap()));
            }
            identity_masks[entry_cnt] = mask;
            entry_cnt += 1;
            debug_assert!(used[2] == false); //See above
            used[1] = true;
            used[2] = true;
        },
        AlgebraicResult::None => {}
    }
    match try_merge::<V, A, 1, 1>(self_key1, a, other_key1, b) {
        AlgebraicResult::Element(joined) => {
            entries[entry_cnt] = MaybeUninit::new(joined);
            entry_cnt += 1;
            debug_assert!(used[1] == false); //See above
            debug_assert!(used[3] == false); //See above
            used[1] = true;
            used[3] = true;
        },
        AlgebraicResult::Identity(mask) => {
            if mask & SELF_IDENT > 0 {
                entries[entry_cnt] = MaybeUninit::new((self_key1, a.clone_payload::<1>().unwrap()));
            } else {
                debug_assert!(mask & COUNTER_IDENT > 0);
                entries[entry_cnt] = MaybeUninit::new((other_key1, b.clone_payload::<1>().unwrap()));
            }
            identity_masks[entry_cnt] = mask;
            entry_cnt += 1;
            debug_assert!(used[1] == false); //See above
            debug_assert!(used[3] == false); //See above
            used[1] = true;
            used[3] = true;
        },
        AlgebraicResult::None => {}
    }

    //Add the single entries that didn't merge
    if !used[0] {
        match a.clone_payload::<0>() {
            Some(payload) => {
                entries[entry_cnt] = MaybeUninit::new((self_key0, payload));
                identity_masks[entry_cnt] = SELF_IDENT;
                entry_cnt += 1;
            },
            None => {}
        }
    }
    if !used[1] {
        match a.clone_payload::<1>() {
            Some(payload) => {
                entries[entry_cnt] = MaybeUninit::new((self_key1, payload));
                identity_masks[entry_cnt] = SELF_IDENT;
                entry_cnt += 1;
            },
            None => {}
        }
    }
    if !used[2] {
        match b.clone_payload::<0>() {
            Some(payload) => {
                entries[entry_cnt] = MaybeUninit::new((other_key0, payload));
                identity_masks[entry_cnt] = COUNTER_IDENT;
                entry_cnt += 1;
            },
            None => {}
        }
    }
    if !used[3] {
        match b.clone_payload::<1>() {
            Some(payload) => {
                entries[entry_cnt] = MaybeUninit::new((other_key1, payload));
                identity_masks[entry_cnt] = COUNTER_IDENT;
                entry_cnt += 1;
            },
            None => {}
        }
    }

    //Do we have two or fewer paths, that can fit into a new ListNode?
    if entry_cnt <= 2 {
        let mut joined_node = LineListNode::new_in(a.alloc.clone());
        let mut pair0: MaybeUninit<(&[u8], ValOrChild<V, A>)> = MaybeUninit::uninit();
        core::mem::swap(&mut pair0, &mut entries[0]);
        let (key0, payload0) = unsafe{ pair0.assume_init() };

        match entry_cnt {
            1 => {
                if identity_masks[0] > 0 {
                    return Ok(AlgebraicResult::Identity(identity_masks[0]))
                } else {
                    unsafe{ joined_node.set_payload_owned::<0>(key0, payload0); }
                    debug_assert!(validate_node(&joined_node));
                    return Ok(AlgebraicResult::Element(joined_node))
                }
            },
            2 => {
                let mut pair1: MaybeUninit<(&[u8], ValOrChild<V, A>)> = MaybeUninit::uninit();
                core::mem::swap(&mut pair1, &mut entries[1]);
                let (key1, payload1) = unsafe{ pair1.assume_init() };
                let new_ident_mask = identity_masks[0] & identity_masks[1];
                if new_ident_mask > 0 {
                    return Ok(AlgebraicResult::Identity(new_ident_mask))
                } else {
                    if should_swap_keys(key0, key1) {
                        unsafe{ joined_node.set_payload_owned::<0>(key1, payload1); }
                        unsafe{ joined_node.set_payload_owned::<1>(key0, payload0); }
                    } else {
                        unsafe{ joined_node.set_payload_owned::<0>(key0, payload0); }
                        unsafe{ joined_node.set_payload_owned::<1>(key1, payload1); }
                    }
                    debug_assert!(validate_node(&joined_node));
                    return Ok(AlgebraicResult::Element(joined_node))
                }
            },
            0 => {
                debug_assert!(a.node_is_empty() && b.node_is_empty());
                return Ok(AlgebraicResult::None)
            },
            _ => unreachable!()
        }
    }

    //Otherwise, create a DenseByteNode
    let mut joined_node = DenseByteNode::<V, A>::with_capacity_in(entry_cnt, a.alloc.clone());
    for i in 0..entry_cnt {
        let mut pair: MaybeUninit<(&[u8], ValOrChild<V, A>)> = MaybeUninit::uninit();
        core::mem::swap(&mut pair, &mut entries[i]);
        let (key, payload) = unsafe{ pair.assume_init() };
        debug_assert!(key.len() > 0);
        if key.len() > 1 {
            let mut child_node = LineListNode::new_in(a.alloc.clone());
            unsafe{ child_node.set_payload_owned::<0>(&key[1..], payload); }
            debug_assert!(validate_node(&child_node));
            joined_node.join_child_into(key[0], TrieNodeODRc::new_in(child_node, a.alloc.clone()));
        } else {
            joined_node.set_payload_owned(key[0], payload);
        }
    }
    Err(AlgebraicResult::Element(joined_node))
}

fn merge_into_list_nodes<V: Clone + Send + Sync + Lattice, A: Allocator>(target: &mut LineListNode<V, A>, other: &LineListNode<V, A>) -> (AlgebraicStatus, Result<(), TrieNodeODRc<V, A>>) {
    match merge_list_nodes(target, other) {
        Ok(AlgebraicResult::Element(new_list_node)) => {
            *target = new_list_node;
            (AlgebraicStatus::Element, Ok(()))
        },
        Ok(AlgebraicResult::Identity(mask)) => {
            if mask & SELF_IDENT > 0 {
                (AlgebraicStatus::Identity, Ok(()))
            } else {
                debug_assert!(mask & COUNTER_IDENT > 0);
                *target = other.clone();
                (AlgebraicStatus::Element, Ok(()))
            }
        },
        Err(AlgebraicResult::Element(new_dense_node)) => (AlgebraicStatus::Element, Err(TrieNodeODRc::new_in(new_dense_node, target.alloc.clone()))),
        _ => unreachable!() //Each case enumerated below
        // Ok(AlgebraicResult::None) => unreachable!(), //Join results are always a superset of self
        // Err(AlgebraicResult::None) => unreachable!(), //Join results are always a superset of self
        // Err(AlgebraicResult::Identity(_)) => unreachable!(), //A new node type can't be an identity
    }
}

fn follow_path<'a, 'k, V: Clone + Send + Sync, A: Allocator>(mut node: &'a dyn TrieNode<V, A>, mut key: &'k[u8]) -> Option<(&'k[u8], &'a dyn TrieNode<V, A>)> {
    while let Some((consumed_byte_cnt, next_node)) = node.node_get_child(key) {
        let next_node = next_node.borrow();
        if consumed_byte_cnt < key.len() {
            node = next_node;
            key = &key[consumed_byte_cnt..]
        } else {
            return Some((key, node));
        };
    }
    if node.node_contains_partial_key(key) {
        Some((key, node))
    } else {
        None
    }
}

/// Follows a path from a node, returning `(true, _)` if a value was encountered along the path, returns
/// `(false, Some)` if the path continues, and `(false, None)` if the path does not descend from the node
fn follow_path_to_value<'a, 'k, V: Clone + Send + Sync, A: Allocator>(mut node: &'a dyn TrieNode<V, A>, mut key: &'k[u8]) -> (bool, Option<(&'k[u8], &'a dyn TrieNode<V, A>)>) {
    while let Some((consumed_byte_cnt, next_node)) = node.node_get_child(key) {
        if consumed_byte_cnt < key.len() {
            let next_node = next_node.borrow();
            node = next_node;
            key = &key[consumed_byte_cnt..]
        } else {
            return (false, Some((key, node)));
        };
    }
    if let Some(_) = node.node_first_val_depth_along_key(key) {
        return (true, None);
    }
    if node.node_contains_partial_key(key) {
        (false, Some((key, node)))
    } else {
        (false, None)
    }
}

impl<V: Clone + Send + Sync, A: Allocator> TrieNode<V, A> for LineListNode<V, A> {
    #[inline]
    fn node_key_overlap(&self, key: &[u8]) -> usize {
        let (key0, key1) = self.get_both_keys();
        let overlap0 = find_prefix_overlap(key, key0);
        let overlap1 = find_prefix_overlap(key, key1);
        overlap0.max(overlap1)
    }
    #[inline]
    fn node_contains_partial_key(&self, key: &[u8]) -> bool {
        let (key0, key1) = self.get_both_keys();
        if key0.starts_with(key) || key1.starts_with(key) {
            return true;
        }
        false
    }
    #[inline(always)]
    fn node_get_child(&self, key: &[u8]) -> Option<(usize, &TrieNodeODRc<V, A>)> {
        if self.is_used_child_0() {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            let key_len = node_key_0.len();
            if key.len() >= key_len {
                if node_key_0 == &key[..key_len] {
                    let child = unsafe{ self.child_in_slot::<0>() };
                    return Some((key_len, child))
                }
            }
        }
        if self.is_used_child_1() {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            let key_len = node_key_1.len();
            if key.len() >= key_len {
                if node_key_1 == &key[..key_len] {
                    let child = unsafe{ self.child_in_slot::<1>() };
                    return Some((key_len, child))
                }
            }
        }
        None
    }
    //GOAT, Deprecated node_get_child_and_val_mut
    // fn node_get_child_and_val_mut(&mut self, key: &[u8]) -> Option<(usize, &mut TrieNodeODRc<V>, &mut Option<V>)> {
    //     unimplemented!()
    //     //GOAT, this code is unsound because we alias the self mutable ref.  To fix it, we need to unpack
    //     // the implementations of get_child_mut and get_val_mut.
    //     //  But we may not actually need this method to work.
    //     //
    //     // let self_ptr: *mut Self = self;
    //     // if let Some((consumed_bytes, child)) = self.get_child_mut(key) {
    //     //     // SAFETY: We know the value and the child will be in different non-overlapping parts of the node,
    //     //     // so it will be safe to mutably borrow them both at the same time.
    //     //     let self_ref = unsafe{ &mut *self_ptr };
    //     //     if let Some(val) = self_ref.get_val_mut(&key[..consumed_bytes]) {
    //     //         Some((consumed_bytes, child, Some(val)))
    //     //     } else {
    //     //         Some((consumed_bytes, None, Some(child)))
    //     //     }
    //     // } else {
    //     //     // SAFETY: In addition to the point above about the value not overlapping the child ptr, we also
    //     //     // drop the previous borrow so this unsafe would be unnecessary under Polonius
    //     //     let self_clone = unsafe{ &mut *self_ptr };
    //     //     if let Some(val) = self_clone.get_val_mut(key) {
    //     //         Some((key.len(), Some(val), None))
    //     //     } else {
    //     //         None
    //     //     }
    //     // }
    // }
    fn node_get_child_mut(&mut self, key: &[u8]) -> Option<(usize, &mut TrieNodeODRc<V, A>)> {
        self.get_child_mut(key)
    }
    //GOAT, we probably don't need this interface, although it is fully implemented and working
    // fn node_contains_children_exclusive(&self, keys: &[&[u8]]) -> bool {
    //     let (key0, key1) = self.get_both_keys();
    //     let mut pos = 0;
    //     if self.is_used_child_0() {
    //         pos = match keys.binary_search(&key0) {
    //             Ok(pos) => pos,
    //             Err(_) => return false
    //         };
    //     }
    //     if self.is_used_child_1() {
    //         match &keys[pos+1..].binary_search(&key1) {
    //             Ok(_) => {},
    //             Err(_) => return false
    //         };
    //     }
    //     true
    // }
    fn node_replace_child(&mut self, key: &[u8], new_node: TrieNodeODRc<V, A>) -> &mut dyn TrieNode<V, A> {
        let (consumed_bytes, child_node) = self.get_child_mut(key).unwrap();
        debug_assert!(consumed_bytes == key.len());
        *child_node = new_node;
        child_node.make_mut()
    }
    fn node_get_payloads<'node, 'res>(&'node self, keys: &[(&[u8], bool)], results: &'res mut [(usize, PayloadRef<'node, V, A>)]) -> bool {
        //GOAT, this code below is correct as far as I know, any will likely be useful in the future when we add additional
        // node types.  But currently there is no path to call it.
        // unreachable!();
        let mut slot_0_requested = !self.is_used::<0>();
        let mut slot_1_requested = !self.is_used::<1>();
        let (node_key_0, node_key_1) = self.get_both_keys();

        debug_assert!(results.len() >= keys.len());
        for ((key, expect_val), (result_key_len, payload_ref)) in keys.into_iter().zip(results.iter_mut()) {
            if self.is_used::<0>() {
                if key.starts_with(node_key_0) {
                    let node_key_len = node_key_0.len();
                    if self.is_child_ptr::<0>() {
                        if !*expect_val || node_key_len < key.len() {
                            slot_0_requested = true;
                            *result_key_len = node_key_len;
                            *payload_ref = PayloadRef::Child(unsafe{ &*self.val_or_child0.child });
                        }
                    } else {
                        if *expect_val && node_key_len == key.len() {
                            slot_0_requested = true;
                            *result_key_len = node_key_len;
                            *payload_ref = PayloadRef::Val(unsafe{ &**self.val_or_child0.val });
                        }
                    }
                }
            }
            if self.is_used::<1>() {
                if key.starts_with(node_key_1) {
                    let node_key_len = node_key_1.len();
                    if self.is_child_ptr::<1>() {
                        if !*expect_val || node_key_len < key.len() {
                            slot_1_requested = true;
                            *result_key_len = node_key_len;
                            *payload_ref = PayloadRef::Child(unsafe{ &*self.val_or_child1.child });
                        }
                    } else {
                        if *expect_val && node_key_len == key.len() {
                            slot_1_requested = true;
                            *result_key_len = node_key_len;
                            *payload_ref = PayloadRef::Val(unsafe{ &**self.val_or_child1.val });
                        }
                    }
                }
            }
        }
        slot_0_requested && slot_1_requested
    }
    fn node_contains_val(&self, key: &[u8]) -> bool {
        self.contains_val(key)
    }
    //GOAT, we probably don't need this interface, although it is fully implemented and working
    // fn node_contains_vals_exclusive(&self, keys: &[&[u8]]) -> bool {
    //     let (key0, key1) = self.get_both_keys();
    //     let mut pos = 0;
    //     if self.is_used_value_0() {
    //         pos = match keys.binary_search(&key0) {
    //             Ok(pos) => pos,
    //             Err(_) => return false
    //         };
    //     }
    //     if self.is_used_value_1() {
    //         match keys[pos+1..].binary_search(&key1) {
    //             Ok(_) => {},
    //             Err(_) => return false
    //         };
    //     }
    //     true
    // }
    fn node_get_val(&self, key: &[u8]) -> Option<&V> {
        self.get_val(key)
    }
    fn node_get_val_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        self.get_val_mut(key)
    }
    fn node_set_val(&mut self, key: &[u8], val: V) -> Result<(Option<V>, bool), TrieNodeODRc<V, A>> {
        self.set_payload_abstract::<false>(key, val.into()).map(|(result, created_subnode)| {
            (result.map(|payload| payload.into_val() ), created_subnode)
        })
    }

    fn node_remove_val(&mut self, key: &[u8]) -> Option<V> {
        if self.is_used_value_0() {
            let node_key_0 = unsafe{ self.key_unchecked::<0>() };
            if node_key_0 == key {
                return Some(self.take_payload::<0>().unwrap().into_val())
            }
        }
        if self.is_used_value_1() {
            let node_key_1 = unsafe{ self.key_unchecked::<1>() };
            if node_key_1 == key {
                return Some(self.take_payload::<1>().unwrap().into_val())
            }
        }
        None
    }

    fn node_set_branch(&mut self, key: &[u8], new_node: TrieNodeODRc<V, A>) -> Result<bool, TrieNodeODRc<V, A>> {
        self.set_payload_abstract::<true>(key, new_node.into())
            .map(|(_, created_subnode)| created_subnode)
    }

    fn node_remove_all_branches(&mut self, key: &[u8]) -> bool {
        let key_len = key.len();
        let (key0, key1) = self.get_both_keys();
        let remove_0 = key0.starts_with(key) && (key0.len() > key_len || self.is_child_ptr::<0>());
        let remove_1 = key1.starts_with(key) && (key1.len() > key_len || self.is_child_ptr::<1>());
        if remove_1 {
            self.take_payload::<1>().unwrap();
        }
        if remove_0 {
            self.take_payload::<0>().unwrap();
        }
        remove_0 || remove_1
    }

    fn node_remove_unmasked_branches(&mut self, key: &[u8], mask: ByteMask) {
        let key_len = key.len();
        let (key0, key1) = self.get_both_keys();
        let mut remove_0 = false;
        let mut remove_1 = false;
        if key0.starts_with(key) {
            if key0.len() > key_len {
                remove_0 = !mask.test_bit(key0[key_len]);
            } else {
                //We can only get here if key0 == key, and the calling code should have descend
                // through this node if that key specifies an onward link
                debug_assert!(!self.is_child_ptr::<0>());
            }
        }
        if key1.starts_with(key) {
            if key1.len() > key_len {
                remove_1 = !mask.test_bit(key1[key_len]);
            } else {
                debug_assert!(!self.is_child_ptr::<1>()); //See comment above
            }
        }
        if remove_1 {
            self.take_payload::<1>().unwrap();
        }
        if remove_0 {
            self.take_payload::<0>().unwrap();
        }
    }

    fn node_is_empty(&self) -> bool {
        !self.is_used::<0>()
    }

    // *==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==*
    // * Explanation of the meaning of iter_tokens for ListNode
    // *
    // * 0 = iteration has not yet begun, so the next call to `next_items` will return the first
    // *   item(s) within the node.
    // * 1 = the item in slot0 has already been returned, so the next call to `next_items` will examine
    // *   slot1.  If slot0 and slot1 have identical keys, iter_tokens==1 will be skipped
    // * 2 = the item in slot1 has already been returned, so the next call to `next_items` must return
    // *   NODE_ITER_FINISHED
    // *==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==**==--==*
    #[inline(always)]
    fn new_iter_token(&self) -> u128 {
        0
    }
    /// Explanation of logic: The ListNode contains a sorted list of keys (up to 2 of them), and the
    /// query `key` argument is a third key.  We want to determine where the `key` arg falls in the sorted
    /// list.
    /// - < key0, we want to return (0, &[])
    /// - >= key0 && < key1, we should return (1, key0)
    /// - == key1, we should return (2, key1)
    /// - > key1, (NODE_ITER_FINISHED, &[])
    #[inline(always)]
    fn iter_token_for_path(&self, key: &[u8]) -> u128 {
        if key.len() == 0 {
            return 0
        }
        let (key0, key1) = self.get_both_keys();
        if key < key0 {
            return 0
        }
        if key < key1 {
            return 1
        }
        if key == key1 {
            return 2
        }
        NODE_ITER_FINISHED
    }
    #[inline(always)]
    fn next_items(&self, token: u128) -> (u128, &[u8], Option<&TrieNodeODRc<V, A>>, Option<&V>) {
        match token {
            0 => {
                if !self.is_used::<0>() {
                    return (NODE_ITER_FINISHED, &[], None, None)
                }
                let (key0, key1) = self.get_both_keys();
                let mut child = None;
                let mut value = None;
                let mut next_token = 1;
                if self.is_child_ptr::<0>() {
                    child = Some(unsafe{ self.child_in_slot::<0>() })
                } else {
                    value = Some(unsafe{ self.val_in_slot::<0>() })
                };
                if key0 == key1 {
                    if self.is_child_ptr::<1>() {
                        child = Some(unsafe{ self.child_in_slot::<1>() });
                    } else {
                        value = Some(unsafe{ self.val_in_slot::<1>() });
                    }
                    next_token = 2;
                }
                (next_token, key0, child, value)
            },
            1 => {
                if self.is_used::<1>() {
                    let mut child = None;
                    let mut value = None;
                    let key1 = unsafe{ self.key_unchecked::<1>() };
                    if self.is_child_ptr::<1>() {
                        child = Some(unsafe{ self.child_in_slot::<1>() });
                    } else {
                        value = Some(unsafe{ self.val_in_slot::<1>() });
                    }
                    (2, key1, child, value)
                } else {
                    (NODE_ITER_FINISHED, &[], None, None)
                }
            },
            _ => (NODE_ITER_FINISHED, &[], None, None)
        }
    }
    fn node_val_count(&self, cache: &mut HashMap<*const dyn TrieNode<V, A>, usize>) -> usize {
        let mut result = 0;
        if self.is_used_value_0() {
            result += 1;
        }
        if self.is_used_value_1() {
            result += 1;
        }
        if self.is_used_child_0() {
            let child_node = unsafe{ self.child_in_slot::<0>() };
            result += val_count_below_node(child_node, cache);
        }
        if self.is_used_child_1() {
            let child_node = unsafe{ self.child_in_slot::<1>() };
            result += val_count_below_node(child_node, cache);
        }
        result
    }
    #[cfg(feature = "counters")]
    fn item_count(&self) -> usize {
        self.count()
    }
    fn node_first_val_depth_along_key(&self, key: &[u8]) -> Option<usize> {
        debug_assert!(key.len() > 0);
        let (key0, key1) = self.get_both_keys();
        if self.is_used_value_0() && key.starts_with(key0) {
            return Some(key0.len() - 1)
        }
        if self.is_used_value_1() && key.starts_with(key1) {
            return Some(key1.len() - 1)
        }
        None
    }
    fn nth_child_from_key(&self, key: &[u8], n: usize) -> (Option<u8>, Option<&dyn TrieNode<V, A>>) {

        //If `n==1` we know the only way we will find a valid result is if it's in slot_1.  On the other
        // hand, if `n==0` we might find the result in slot_0, or it might be in slot_1 because the key in
        // slot_0 doesn't match the key being passed.
        match n {
            0 => {
                let (key0, key1) = self.get_both_keys();
                if key0.starts_with(key) && key0.len() > key.len() {
                    if key0 != key1 {
                        if key.len() + 1 == key0.len() && self.is_child_ptr::<0>() {
                            return (Some(key0[key.len()]), unsafe{ Some(self.child_in_slot::<0>().borrow()) })
                        } else {
                            return (Some(key0[key.len()]), None)
                        }
                    }
                }
                if key1.starts_with(key) && key1.len() > key.len() {
                    if key.len() + 1 == key1.len() && self.is_child_ptr::<1>() {
                        return (Some(key1[key.len()]), unsafe{ Some(self.child_in_slot::<1>().borrow()) })
                    } else {
                        return (Some(key1[key.len()]), None)
                    }
                }
            }
            1 => {
                if self.is_used::<1>() {
                    //The only way we can get a valid child branch at index 1 is if key_length == 0,
                    // because ListNode has a rule that overlap is only allowed at the first byte
                    if key.len() > 0 {
                        return (None, None)
                    }
                    let (key0, key1) = self.get_both_keys();
                    if key1.len() > 0 {
                        if key0[0] == key1[0] {
                            return (None, None)
                        }
                        if key1.len() == 1 && self.is_child_ptr::<1>() {
                            return (Some(key1[key.len()]), unsafe{ Some(self.child_in_slot::<1>().borrow()) })
                        } else {
                            return (Some(key1[key.len()]), None)
                        }
                    }
                }
            },
            _ => {}
        }
        (None, None)
    }

    fn first_child_from_key(&self, key: &[u8]) -> (Option<&[u8]>, Option<&dyn TrieNode<V, A>>) {
        //Logic:  There are 6 possible results from this method:
        // 1. The `key` arg is zero-length, in which case this method should return the common prefix
        //    if there is one (which is guaranteed to be one byte), or otherwise return the result in slot0
        // 2. The supplied key exactly matches key0 and key1.  In this case, the result is whichever of the
        //    two results is an onward node link. This case can only occur if the `key` arg length is 1.
        // 3. The supplied key exactly matches key0 and is a prefix of key1.  The result is the remaining
        //   bytes from key1 and the onward link from slot1.  This also can only happen if the
        //   `key` arg length is 1.
        // 4. The supplied key is a prefix of key0.  The result is the remaining bytes
        //   from key0 and the onward link from slot0
        // 5. The supplied key is a prefix of key1.  The result is the remaining bytes
        //   from key1 and the onward link from slot1
        // 6. The supplied key meets none of the above.  The result is (None, None)

        let (key0, key1) = self.get_both_keys();

        //Case 1
        if key.len() == 0 {
            if key1.len() > 0 && key0[0] == key1[0] {
                if key0.len() == 1 && self.is_child_ptr::<0>() {
                    return (Some(key0), unsafe{ Some(self.child_in_slot::<0>().borrow()) });
                }
                if key1.len() == 1 && self.is_child_ptr::<1>() {
                    return (Some(key0), unsafe{ Some(self.child_in_slot::<1>().borrow()) });
                }
                return (Some(&key0[0..1]), None);
            } else {
                if self.is_child_ptr::<0>() {
                    return (Some(key0), unsafe{ Some(self.child_in_slot::<0>().borrow()) })
                } else {
                    return (Some(key0), None)
                }
            }
        }

        if key.len() == 1 && key0.len() == 1 && key0[0] == key[0] {

            //Case 2
            if key1.len() == 1 && key1[0] == key[0] {
                if self.is_child_ptr::<0>() {
                    return (Some(&[]), unsafe{ Some(self.child_in_slot::<0>().borrow()) })
                }
                if self.is_child_ptr::<1>() {
                    return (Some(&[]), unsafe{ Some(self.child_in_slot::<1>().borrow()) })
                } else {
                    unreachable!(); //If the node has identical keys, one of them must be a link
                }
            }

            //Case 3
            if key1.len() > 1 && key1[0] == key[0] {
                let remaining_key = remaining_key(key1, 1);
                if self.is_child_ptr::<1>() {
                    return (Some(remaining_key), unsafe{ Some(self.child_in_slot::<1>().borrow()) })
                } else {
                    return (Some(remaining_key), None)
                }
            }
        }

        //Case 4
        if key0.starts_with(key) {
            let remaining_key = remaining_key(key0, key.len());
            if self.is_child_ptr::<0>() {
                return (Some(remaining_key), unsafe{ Some(self.child_in_slot::<0>().borrow()) })
            } else {
                return (Some(remaining_key), None)
            }
        }

        //Case 5
        if key1.starts_with(key) {
            let remaining_key = remaining_key(key1, key.len());
            if self.is_child_ptr::<1>() {
                return (Some(remaining_key), unsafe{ Some(self.child_in_slot::<1>().borrow()) })
            } else {
                return (Some(remaining_key), None)
            }
        }

        //Case 6
        (None, None)
    }

    fn count_branches(&self, key: &[u8]) -> usize {
        let key_len = key.len();
        let (key0, key1) = self.get_both_keys();

        // The logic here is tricky, primarily because keys and values are represented
        // separately in the list with overlapping keys
        //
        // k0="h", k1="hi", key="", result = 1
        // k0="h", k1="hi", key="h", result = 1
        // k0="h", k1="hi", key="hi", result = 0, because "hi" must be a value, otherwise the node would have advanced to the next node
        // k0="ahoy", k1="howdy", key="h", result = 1
        // k0="ahoy", k1="howdy", key="", result = 2

        let c0 = if key0.len() > key_len && key0.starts_with(key) {
            Some(key0[key_len])
        } else {
            None
        };
        let c1 = if key1.len() > key_len && key1.starts_with(key) {
            Some(key1[key_len])
        } else {
            None
        };
        match (c0, c1) {
            (None, None) => 0,
            (Some(_), None) => 1,
            (None, Some(_)) => 1,
            (Some(c0), Some(c1)) => {
                if c0 == c1 {
                    1
                } else {
                    2
                }
            }
        }
    }

    #[inline(always)]
    fn node_branches_mask(&self, key: &[u8]) -> ByteMask {
        let (key0, key1) = self.get_both_keys();
        let mut m = [0u64; 4];

        if key0.len() > key.len() && key0.starts_with(key) {
            let k = key0[key.len()];
            m[((k & 0b11000000) >> 6) as usize] |= 1u64 << (k & 0b00111111);
        }
        if key1.len() > key.len() && key1.starts_with(key) {
            let k = key1[key.len()];
            m[((k & 0b11000000) >> 6) as usize] |= 1u64 << (k & 0b00111111);
        }
        m.into()
    }

    fn is_leaf(&self, key: &[u8]) -> bool {
        let key_len = key.len();
        let (key0, key1) = self.get_both_keys();
        if key0.starts_with(key) {
            if key_len < key0.len() || self.is_child_ptr::<0>() {
                return false;
            }
        }
        if key1.starts_with(key) {
            if key_len < key1.len() || self.is_child_ptr::<1>() {
                return false;
            }
        }
        true
    }

    fn prior_branch_key(&self, key: &[u8]) -> &[u8] {
        debug_assert!(key.len() > 0);

        //The key-add logic elsewhere in this file would have split the node if the overlap between the keys
        // were more than one character. However list-node keys are allowed to have the first character in
        // common to avoid the possibility of needing zero-length keys.
        //Therefore there are 3 possible cases:
        // - Case0 - key is a superset of one of the existing keys, so the result is the existing key
        // - Case1 - key.len() > 1 and the node keys' first bytes are the same and therfore we have a 1-byte banch key or
        // - Case2 - key.len() == 1, or the node keys' first bytes are different, in which case we have a zero-length branch key
        let key_len = key.len();
        if key_len == 1 {
            return &[]
        }
        let (key0, key1) = self.get_both_keys();
        if key_len > key0.len() {
            if &key[..key0.len()] == key0 {
                return key0
            }
        }
        if key_len > key1.len() {
            if &key[..key1.len()] == key1 {
                return key1
            }
        }
        let key_byte = key.get(0);
        if key0.get(0) == key_byte && key1.get(0) == key_byte {
            &key0[0..1]
        } else {
            &[]
        }
    }

    fn get_sibling_of_child(&self, key: &[u8], next: bool) -> (Option<u8>, Option<&dyn TrieNode<V, A>>) {
        debug_assert!(key.len() > 0);
        let last_key_byte_idx = key.len()-1;
        let common_key = &key[..last_key_byte_idx];
        let (key0, key1) = self.get_both_keys();
        match next {
            true => {
                if key0.starts_with(key) && key1.starts_with(common_key) {
                    let key1_last_byte = match key1.get(last_key_byte_idx) {
                        Some(byte) => byte,
                        None => return (None, None)
                    };
                    if key1_last_byte != key.last().unwrap() {
                        let sib_node = if key1.len() == key.len() && self.is_child_ptr::<1>() {
                            let sib_node = unsafe{ self.child_in_slot::<1>().borrow() };
                            debug_assert!({ sib_node.as_tagged().as_list().map(|sib_node| validate_node(sib_node)); true});
                            Some(sib_node)
                        } else {
                            None
                        };
                        (Some(*key1_last_byte), sib_node)
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                }
            },
            false => {
                if key1.starts_with(key) && key0.starts_with(common_key) {
                    let key0_last_byte = match key0.get(last_key_byte_idx) {
                        Some(byte) => byte,
                        None => return (None, None)
                    };
                    if key0_last_byte != key.last().unwrap() {
                        let sib_node = if key0.len() == key.len() && self.is_child_ptr::<0>() {
                            let sib_node = unsafe{ self.child_in_slot::<0>().borrow() };
                            debug_assert!({ sib_node.as_tagged().as_list().map(|sib_node| validate_node(sib_node)); true});
                            Some(sib_node)
                        } else {
                            None
                        };
                        (Some(*key0_last_byte), sib_node)
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                }
            }
        }
    }

    fn get_node_at_key(&self, key: &[u8]) -> AbstractNodeRef<'_, V, A> {
        debug_assert!(validate_node(self));

        //Zero-length key means clone this node
        if key.len() == 0 {
            return if !self.node_is_empty() {
                AbstractNodeRef::BorrowedDyn(self)
            } else {
                AbstractNodeRef::None
            }
        }
        //Exact match with a path to a child node means return that node
        let (key0, key1) = self.get_both_keys();
        if self.is_used_child_0() && key0 == key {
            return AbstractNodeRef::BorrowedRc(unsafe{ self.child_in_slot::<0>() })
        }
        if self.is_used_child_1() && key1 == key {
            return AbstractNodeRef::BorrowedRc(unsafe{ self.child_in_slot::<1>() })
        }
        //Otherwise check to see if we need to make a sub-node.  If we do,
        // We know the new node will have only 1 slot filled
        if key0.len() > key.len() && key0.starts_with(key) {
            let new_key = &key0[key.len()..];
            //If the new node's key is 7 Bytes or fewer, we can make a TinyRefNode
            if new_key.len() <= 7 {
                let ref_node = TinyRefNode::new_in(self.is_child_ptr::<0>(), new_key, &self.val_or_child0, self.alloc.clone());
                return AbstractNodeRef::BorrowedTiny(ref_node);
            } else {
                let mut new_node = Self::new_in(self.alloc.clone());
                let payload = self.clone_payload::<0>().unwrap();
                unsafe{ new_node.set_payload_owned::<0>(new_key, payload); }
                debug_assert!(validate_node(&new_node));
                return AbstractNodeRef::OwnedRc(TrieNodeODRc::new_in(new_node, self.alloc.clone()));
            }
        }
        if key1.len() > key.len() && key1.starts_with(key) {
            let new_key = &key1[key.len()..];
            //If the new node's key is 7 Bytes or fewer, we can make a TinyRefNode
            if new_key.len() <= 7 {
                let ref_node = TinyRefNode::new_in(self.is_child_ptr::<1>(), new_key, &self.val_or_child1, self.alloc.clone());
                return AbstractNodeRef::BorrowedTiny(ref_node);
            } else {
                let mut new_node = Self::new_in(self.alloc.clone());
                let payload = self.clone_payload::<1>().unwrap();
                unsafe{ new_node.set_payload_owned::<0>(new_key, payload); }
                debug_assert!(validate_node(&new_node));
                return AbstractNodeRef::OwnedRc(TrieNodeODRc::new_in(new_node, self.alloc.clone()));
            }
        }
        //The key must specify a path the node doesn't contains
        AbstractNodeRef::None
    }

    fn take_node_at_key(&mut self, key: &[u8]) -> Option<TrieNodeODRc<V, A>> {
        debug_assert!(validate_node(self));
        debug_assert!(key.len() > 0);

        //Exact match with a path to a child node means take that node
        let (key0, key1) = self.get_both_keys();
        if self.is_used_child_0() && key0 == key {
            return self.take_payload::<0>().map(|payload| payload.into_child())
        }
        if self.is_used_child_1() && key1 == key {
            return self.take_payload::<1>().map(|payload| payload.into_child())
        }

        //Otherwise check to see if we need to make a sub-node.  If we do,
        // We know the new node will have only 1 slot filled
        if key0.len() > key.len() && key0.starts_with(key) {
            let mut new_node = Self::new_in(self.alloc.clone());
            unsafe{ new_node.set_payload_0(&key0[key.len()..], self.is_child_ptr::<0>(), ValOrChildUnion{ _unused: () }) }
            new_node.val_or_child0 = self.take_payload::<0>().unwrap().into();
            debug_assert!(validate_node(&new_node));
            return Some(TrieNodeODRc::new_in(new_node, self.alloc.clone()));
        }
        if key1.len() > key.len() && key1.starts_with(key) {
            let mut new_node = Self::new_in(self.alloc.clone());
            unsafe{ new_node.set_payload_0(&key1[key.len()..], self.is_child_ptr::<1>(), ValOrChildUnion{ _unused: () }) }
            new_node.val_or_child0 = self.take_payload::<1>().unwrap().into();
            debug_assert!(validate_node(&new_node));
            return Some(TrieNodeODRc::new_in(new_node, self.alloc.clone()));
        }
        None
    }

    fn pjoin_dyn(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<TrieNodeODRc<V, A>> where V: Lattice {
        debug_assert!(validate_node(self));
        match other.as_tagged() {
            TaggedNodeRef::LineListNode(other_list_node) => {
                match merge_list_nodes(self, other_list_node) {
                    Ok(joined_list_node) => joined_list_node.map(|node| TrieNodeODRc::new_in(node, self.alloc.clone())),
                    Err(joined_dense_node) => joined_dense_node.map(|node| TrieNodeODRc::new_in(node, self.alloc.clone())),
                }
            },
            TaggedNodeRef::DenseByteNode(other_dense_node) => {
                let mut new_node = other_dense_node.clone();
                match new_node.merge_from_list_node(self) {
                    AlgebraicStatus::None => unreachable!(), //Joining a non-empty node with another non-empty node should never produce an empty node
                    AlgebraicStatus::Identity => AlgebraicResult::Identity(COUNTER_IDENT),
                    AlgebraicStatus::Element => AlgebraicResult::Element(TrieNodeODRc::new_in(new_node, self.alloc.clone()))
                }
            },
            #[cfg(feature = "bridge_nodes")]
            TaggedNodeRef::BridgeNode(_other_bridge_node) => {
                unimplemented!()
            },
            TaggedNodeRef::TinyRefNode(tiny_node) => {
                tiny_node.pjoin_dyn(self)
            },
            TaggedNodeRef::CellByteNode(other_dense_node) => {
                let mut new_node = other_dense_node.clone();
                match new_node.merge_from_list_node(self) {
                    AlgebraicStatus::None => unreachable!(), //Joining a non-empty node with another non-empty node should never produce an empty node
                    AlgebraicStatus::Identity => AlgebraicResult::Identity(COUNTER_IDENT),
                    AlgebraicStatus::Element => AlgebraicResult::Element(TrieNodeODRc::new_in(new_node, self.alloc.clone()))
                }
            },
            TaggedNodeRef::EmptyNode => {
                AlgebraicResult::Identity(SELF_IDENT)
            }
        }
    }

    fn join_into_dyn(&mut self, other: TrieNodeODRc<V, A>) -> (AlgebraicStatus, Result<(), TrieNodeODRc<V, A>>) where V: Lattice {
        debug_assert!(validate_node(self));
        match other.borrow().as_tagged() {
            TaggedNodeRef::LineListNode(other_list_node) => {
                merge_into_list_nodes(self, other_list_node)
            },
            TaggedNodeRef::DenseByteNode(other_dense_node) => {
                let mut new_node = other_dense_node.clone();
                let status = new_node.merge_from_list_node(self);
                debug_assert!(!status.is_none());
                (AlgebraicStatus::Element, Err(TrieNodeODRc::new_in(new_node, self.alloc.clone())))
            },
            #[cfg(feature = "bridge_nodes")]
            TaggedNodeRef::BridgeNode(_other_bridge_node) => {
                unimplemented!()
            },
            TaggedNodeRef::TinyRefNode(tiny_node) => {
                let other_tiny_node = tiny_node.into_list_node().unwrap();
                merge_into_list_nodes(self, &other_tiny_node)
            },
            TaggedNodeRef::CellByteNode(other_dense_node) => {
                let mut new_node = other_dense_node.clone();
                let status = new_node.merge_from_list_node(self);
                debug_assert!(!status.is_none());
                (AlgebraicStatus::Element, Err(TrieNodeODRc::new_in(new_node, self.alloc.clone())))
            },
            TaggedNodeRef::EmptyNode => (AlgebraicStatus::Identity, Ok(()))
        }
    }

    fn drop_head_dyn(&mut self, byte_cnt: usize) -> Option<TrieNodeODRc<V, A>> where V: Lattice {
        debug_assert!(byte_cnt > 0);

        //If the node has any values with where `key_len <= byte_cnt`, we can discard those values now
        if self.is_used_value_1() && self.key_len_1() <= byte_cnt {
            let _ = self.take_payload::<1>();
        }
        if self.is_used_value_0() && self.key_len_0() <= byte_cnt {
            let _ = self.take_payload::<0>();
        }

        //If the node is empty, we're done
        if !self.is_used::<0>() {
            return None
        }

        //Case for a node with only one slot filled
        if !self.is_used::<1>() {
            let mut temp_node = Self::new_in(self.alloc.clone());
            core::mem::swap(self, &mut temp_node);
            let key_len = temp_node.key_len_0();

            // See if we just shorten the key in this node, or if we need to discard the node entirely and recurse
            if byte_cnt < key_len {
                let new_key_len = key_len-byte_cnt;
                unsafe{
                    let base_ptr = temp_node.key_bytes.as_mut_ptr().cast::<u8>();
                    let src_ptr = base_ptr.add(byte_cnt);
                    let dst_ptr = base_ptr;
                    core::ptr::copy(src_ptr, dst_ptr, new_key_len);
                }
                temp_node.header &= 0xf03f; //Zero out the old length, and reset it
                temp_node.header |= (new_key_len << 6) as u16;
                debug_assert!(validate_node(&temp_node));
                return Some(TrieNodeODRc::new_in(temp_node, self.alloc.clone()))
            } else {
                let remaining_bytes = byte_cnt-key_len;
                debug_assert!(temp_node.is_child_ptr::<0>() == true);
                let mut child = match temp_node.take_payload::<0>().unwrap() {
                    ValOrChild::Child(child) => child,
                    ValOrChild::Val(_) => unreachable!(),
                };
                if remaining_bytes > 0 {
                    return child.make_mut().drop_head_dyn(remaining_bytes)
                } else {
                    return Some(child)
                }
            }
        }

        //If we get here, both slots are filled
        debug_assert_eq!(self.is_used::<0>(), true);
        debug_assert_eq!(self.is_used::<1>(), true);
        let mut temp_node = Self::new_in(self.alloc.clone());
        core::mem::swap(self, &mut temp_node);
        let (key0, key1) = temp_node.get_both_keys();
        let key0_len = key0.len();
        let key1_len = key1.len();

        //If byte_cnt < both key lengths, reuse this node but shorten the keys
        if byte_cnt < key0_len && byte_cnt < key1_len {
            let mut slot0_child = temp_node.is_child_ptr::<0>();
            let mut slot1_child = temp_node.is_child_ptr::<1>();
            let mut new_key0_len = key0_len-byte_cnt;
            let mut new_key1_len = key1_len-byte_cnt;
            //Make sure the new keys are in the correctly sorted order
            if &key0[byte_cnt..] <= &key1[byte_cnt..] {
                unsafe {
                    //Shorten key0
                    let base_ptr = temp_node.key_bytes.as_mut_ptr().cast::<u8>();
                    let src_ptr = base_ptr.add(byte_cnt);
                    let dst_ptr = base_ptr;
                    core::ptr::copy(src_ptr, dst_ptr, new_key0_len);
                    //Shorten key1
                    let src_ptr = base_ptr.add(key0_len+byte_cnt);
                    let dst_ptr = base_ptr.add(new_key0_len);
                    core::ptr::copy(src_ptr, dst_ptr, new_key1_len);
                }
            } else {
                unsafe {
                    //Move key0 into a temp buffer
                    let mut tmp_key_buf: [MaybeUninit<u8>; KEY_BYTES_CNT] = [MaybeUninit::new(0); KEY_BYTES_CNT];
                    let src_ptr = temp_node.key_bytes.as_ptr().cast::<u8>().add(byte_cnt);
                    let dst_ptr = tmp_key_buf.as_mut_ptr().cast::<u8>();
                    core::ptr::copy(src_ptr, dst_ptr, new_key0_len);
                    //Shorten key1 into the key0 slot
                    let base_ptr = temp_node.key_bytes.as_mut_ptr().cast::<u8>();
                    let src_ptr = base_ptr.add(key0_len+byte_cnt);
                    let dst_ptr = base_ptr;
                    core::ptr::copy(src_ptr, dst_ptr, new_key1_len);
                    //Move the temp key into the key1 slot
                    let src_ptr = tmp_key_buf.as_ptr().cast::<u8>();
                    let dst_ptr = temp_node.key_bytes.as_mut_ptr().cast::<u8>().add(new_key1_len);
                    core::ptr::copy(src_ptr, dst_ptr, new_key0_len);
                }
                core::mem::swap(&mut new_key0_len, &mut new_key1_len);
                core::mem::swap(&mut slot0_child, &mut slot1_child);
                core::mem::swap(&mut temp_node.val_or_child0, &mut temp_node.val_or_child1);
            }
            temp_node.header = Self::header0(slot0_child, new_key0_len) | Self::header1(slot1_child, new_key1_len);
            temp_node.factor_prefix();
            debug_assert!(validate_node(&temp_node));
            return Some(TrieNodeODRc::new_in(temp_node, self.alloc.clone()))
        }

        //The final case is to construct a brand new node from the remaining parts of the key after we have
        // discarded what we can discard and then merged together what's left.  And then call this function
        // recursively on the newly merged nodes
        let chop_bytes = key0_len.min(key1_len);
        debug_assert!(chop_bytes <= byte_cnt);
        debug_assert!(chop_bytes > 0);
        let new_key0 = &key0[chop_bytes-1..];
        let new_key1 = &key1[chop_bytes-1..];

        let overlap = find_prefix_overlap(&key0[chop_bytes..], &key1[chop_bytes..]);
        let merged_payload = match merge_guts::<V, A, 0, 1>(overlap+1, new_key0, &temp_node, new_key1, &temp_node) {
            AlgebraicResult::Element((_shared_key, merged_payload)) => merged_payload,
            AlgebraicResult::Identity(mask) => {
                if mask & SELF_IDENT > 0 {
                    temp_node.clone_payload::<0>().unwrap()
                } else {
                    debug_assert_eq!(mask, COUNTER_IDENT);
                    temp_node.clone_payload::<1>().unwrap()
                }
            },
            AlgebraicResult::None => unreachable!() //`merge_guts` shouldn't return AlgebraicResult::None because that should have been caught by an earlier case
        };

        if let ValOrChild::Child(mut child_node) = merged_payload {
            if chop_bytes == byte_cnt {
                return Some(child_node)
            } else {
                return child_node.make_mut().drop_head_dyn(byte_cnt-chop_bytes)
            }
        }

        unreachable!()
    }

    fn pmeet_dyn(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<TrieNodeODRc<V, A>> where V: Lattice {
        debug_assert!(validate_node(self));

        let mut self_payloads_buf: [(&[u8], PayloadRef<V, A>); 2] = [(&[], PayloadRef::None); 2];

        let self_slot_count = self.used_slot_count();
        let self_payloads = match self_slot_count {
            0 => return AlgebraicResult::None,
            1 => {
                let key = unsafe{ self.key_unchecked::<0>() };
                let payload = unsafe{ self.payload_in_slot::<0>() };
                self_payloads_buf[0] = (key, payload);
                &self_payloads_buf[..1]
            },
            2 => {
                let (key0, key1) = self.get_both_keys();
                let payload0 = unsafe{ self.payload_in_slot::<0>() };
                let payload1 = unsafe{ self.payload_in_slot::<1>() };
                self_payloads_buf[0] = (key0, payload0);
                self_payloads_buf[1] = (key1, payload1);
                &self_payloads_buf[..2]
            },
            _ => unsafe{ unreachable_unchecked() }
        };

        pmeet_generic::<2, V, A, _>(self_payloads, other, |payloads| {
            debug_assert_eq!(payloads.len(), self_payloads.len());
            let slot0_payload = payloads.get_mut(0).and_then(|p| core::mem::take(p)).map(|p| p.into());
            let slot1_payload = payloads.get_mut(1).and_then(|p| core::mem::take(p)).map(|p| p.into());
            let new_node = self.clone_with_updated_payloads(slot0_payload, slot1_payload).unwrap();
            TrieNodeODRc::new_in(new_node, self.alloc.clone())
        })
    }
    fn psubtract_dyn(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<TrieNodeODRc<V, A>> where V: DistributiveLattice {
        debug_assert!(validate_node(self));
        let slot0_result = self.subtract_from_slot_contents::<0>(other);
        let slot1_result = self.subtract_from_slot_contents::<1>(other);
        self.combine_slot_results_into_node_result(slot0_result, slot1_result)
    }
    fn prestrict_dyn(&self, other: &dyn TrieNode<V, A>) -> AlgebraicResult<TrieNodeODRc<V, A>> {
        debug_assert!(validate_node(self));
        let slot0_result = self.restrict_slot_contents::<0>(other);
        let slot1_result = self.restrict_slot_contents::<1>(other);
        self.combine_slot_results_into_node_result(slot0_result, slot1_result)
    }
    fn clone_self(&self) -> TrieNodeODRc<V, A> {
        TrieNodeODRc::new_in(self.clone(), self.alloc.clone())
    }
}

impl<V: Clone + Send + Sync, A: Allocator> TrieNodeDowncast<V, A> for LineListNode<V, A> {
    #[inline]
    fn tag(&self) -> usize {
        LINE_LIST_NODE_TAG
    }
    #[inline(always)]
    fn as_tagged(&self) -> TaggedNodeRef<'_, V, A> {
        TaggedNodeRef::LineListNode(self)
    }
    #[inline(always)]
    fn as_tagged_mut(&mut self) -> TaggedNodeRefMut<'_, V, A> {
        TaggedNodeRefMut::LineListNode(self)
    }
    fn convert_to_cell_node(&mut self) -> TrieNodeODRc<V, A> {
        self.convert_to_dense::<CellCoFree<V, A>>(3)
    }
}

/// DEBUG-ONLY  Performs some validity tests to catch malformed ListNodes before they can wreak more havoc
#[cfg(debug_assertions)]
pub(crate) fn validate_node<V: Clone + Send + Sync, A: Allocator>(node: &LineListNode<V, A>) -> bool {
    let (key0, key1) = node.get_both_keys();

    //If a key is used it must be non-zero length
    if node.is_used::<0>() && key0.len() == 0 || node.is_used::<1>() && key1.len() == 0 {
        println!("Invalid node - zero-length key. {node:?}");
        panic!()
    }

    //We are never allowed to have an onward child pointer in slot_0 if the key in slot_1 is a superset of the key in slot_0
    if node.is_used_child_0() && key1.starts_with(key0) && key1.len() > key0.len() {
        println!("Invalid node - ambiguous path violation. {node:?}");
        panic!()
    }

    //If slot_1 is filled, the key in slot_1 may never be a subset of the key in slot_0, only a superset
    if node.is_used::<1>() && key0.len() > key1.len() && key0.starts_with(key1) {
        println!("Invalid node - ordering violation. {node:?}");
        panic!()
    }

    //The keys may never have more than one prefix byte in common
    if key0.get(0) == key1.get(0) && key0.get(1) == key1.get(1) && key0.get(1).is_some() {
        panic!()
    }

    //key0 must always be alphabetically before key1, if slot_1 is filled
    if node.is_used::<1>() && key0 > key1 {
        println!("Invalid node - keys not sorted {node:?}");
        panic!()
    }

    true
}

/// So release build will compile
#[cfg(not(debug_assertions))]
pub(crate) fn validate_node<V: Clone + Send + Sync, A: Allocator>(_node: &LineListNode<V, A>) -> bool { true }

#[cfg(test)]
mod tests {
    use crate::{global_alloc, Allocator, GlobalAlloc};
    use super::*;

    fn get_recursive<'a, 'b, V: Clone + Send + Sync, A: Allocator>(key: &'a [u8], node: &'b dyn TrieNode<V, A>) -> (&'a [u8], &'b dyn TrieNode<V, A>, usize) {
        let mut remaining_key = key;
        let mut child_node = node as &dyn TrieNode<V, A>;
        let mut levels = 0;
        while let Some((bytes_used, next_node)) = child_node.node_get_child(remaining_key) {
            let next_node = next_node.borrow();
            remaining_key = &remaining_key[bytes_used..];
            child_node = next_node;
            levels += 1;
        }
        (remaining_key, child_node, levels)
    }

    /// Common operations for a ListNode
    #[test]
    fn test_line_list_node() {
        // assert_eq!(core::mem::size_of::<LineListNode<[u8; 1024]>>(), 64);
        #[cfg(feature = "slim_ptrs")]
        assert_eq!(core::mem::size_of::<LineListNode<[u8; 1024], GlobalAlloc>>(), 64);
        #[cfg(not(feature = "slim_ptrs"))]
        assert_eq!(core::mem::size_of::<LineListNode<[u8; 1024], GlobalAlloc>>(), 48); //Shrunk to account for DynBox header

        //A simple test with a V that fits inside 16 bytes, only testing slot_0
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("hello".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&42));
        *new_node.node_get_val_mut("hello".as_bytes()).unwrap() = 123;
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&123));
        assert_eq!(new_node.node_set_val("hello".as_bytes(), 42).map_err(|_| 0), Ok((Some(123), false)));
        assert_eq!(new_node.node_contains_val("test".as_bytes()), false);

        //A simple test with a V that exceeds 16 bytes, only testing slot_0
        let mut new_node = LineListNode::<[u128; 4], GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("hello".as_bytes(), [0, 1, 2, 3]).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&[0, 1, 2, 3]));
        *new_node.node_get_val_mut("hello".as_bytes()).unwrap() = [3, 2, 1, 0];
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&[3, 2, 1, 0]));
        assert_eq!(new_node.node_contains_val("test".as_bytes()), false);

        //A test using both slots for values, where the keys are different, but both fit inside the key space
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("hello".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("goodbye".as_bytes(), 24).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&42));
        assert_eq!(new_node.node_get_val("goodbye".as_bytes()), Some(&24));
    }

    #[test]
    fn test_line_list_node_key_and_value_collision() {

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("a".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_get_val("a".as_bytes()), Some(&42));

        let mut child_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(child_node.node_set_val("hello".as_bytes(), 24).map_err(|_| 0), Ok((None, false)));
        //We are manufacturing this case.  Otherwise it would be a lot more indirect to achieve the
        // conditions for this test using the APIs available outside this module
        unsafe{ new_node.set_child_1("a".as_bytes(), TrieNodeODRc::new_in(child_node, global_alloc())); }

        assert_eq!(new_node.node_get_val("a".as_bytes()), Some(&42));
        let (bytes_used, child_node) = new_node.node_get_child("a".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 1);
        assert_eq!(child_node.node_get_val("hello".as_bytes()), Some(&24));
    }

    /// This tests that a common prefix will be found and the if the node only has an entry in slot_0
    #[test]
    fn test_line_list_node_shared_prefixes_empty() {

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("my name is".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("my name is billy".as_bytes(), 24).map_err(|_| 0), Ok((None, true)));
        let (bytes_used, child_node) = new_node.node_get_child("my name is".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 9);
        assert_eq!(child_node.node_get_val("s".as_bytes()), Some(&42));
        assert_eq!(child_node.node_get_val("s billy".as_bytes()), Some(&24));
    }

    /// This tests that a long key gets chopped up into multiple nodes
    #[test]
    fn test_line_list_node_overflow_keys() {

        //A test using both slots, where the second key exceeds the available space.  Make sure recursive nodes
        // are created
        const LONG_KEY: &[u8] = "Pack my box with five dozen liquor jugs".as_bytes();
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("hello".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val(LONG_KEY, 24).map_err(|_| 0), Ok((None, true)));
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&42));

        let (remaining_key, child_node, _) = get_recursive(LONG_KEY, &new_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&24));
    }

    /// This tests the logic to split a single key that consumes a whole node into multiple nodes
    #[test]
    fn test_line_list_overflow_split_in_place() {
        const LONG_KEY: &[u8] = "Pack my box with five dozen liquor jugs. Now is the time for all good men to come to the aid of their country.".as_bytes();

        //A test using only one slot, where the key exceeds the available space, make sure recursive nodes
        // are created
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val(LONG_KEY, 24).map_err(|_| 0), Ok((None, true)));
        let (remaining_key, child_node, levels) = get_recursive(LONG_KEY, &new_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&24));
        assert_eq!(levels, (LONG_KEY.len()-1) / KEY_BYTES_CNT);

        //Now make sure that adding a second key is still ok because of in-place splitting
        assert_eq!(new_node.node_set_val("hello".as_bytes(), 42).map_err(|_| 0), Ok((None, true)));
        assert_eq!(new_node.node_get_val("hello".as_bytes()), Some(&42));
    }

    /// This tests that a common prefix is found with the entry in slot_0, when slot_1 is already full
    #[test]
    fn test_line_list_node_shared_prefixes_slot_0() {

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("I'm billy".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("slot1".as_bytes(), 123).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("I'm johnny".as_bytes(), 24).map_err(|_| 0), Ok((None, true)));
        let (bytes_used, child_node) = new_node.node_get_child("I'm billy".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 4);
        assert_eq!(child_node.node_get_val("billy".as_bytes()), Some(&42));
        assert_eq!(child_node.node_get_val("johnny".as_bytes()), Some(&24));
        assert_eq!(new_node.node_get_val("slot1".as_bytes()), Some(&123));
    }

    /// This test consumes slot_0, and tests that a common prefix is found when adding an entry to slot_1
    #[test]
    fn test_line_list_node_shared_prefixes_slot_1() {

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("slot0".as_bytes(), 123).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("I'm billy".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("I'm johnny".as_bytes(), 24).map_err(|_| 0), Ok((None, true)));
        let (bytes_used, child_node) = new_node.node_get_child("I'm billy".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 4);
        assert_eq!(child_node.node_get_val("billy".as_bytes()), Some(&42));
        assert_eq!(child_node.node_get_val("johnny".as_bytes()), Some(&24));
        assert_eq!(new_node.node_get_val("slot0".as_bytes()), Some(&123));
    }

    #[test]
    fn test_line_list_node_replacement() {

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("apple".as_bytes(), 1).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("banana".as_bytes(), 2).map_err(|_| 0), Ok((None, false)));
        let replacement_node = new_node.node_set_val("carrot".as_bytes(), 3).unwrap_err();
        if let TaggedNodeRef::DenseByteNode(_) = replacement_node.borrow().as_tagged() { } else { panic!("expected node would be a byte node"); }
        let (bytes_used, child_node) = replacement_node.borrow().node_get_child("apple".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 1);
        assert_eq!(child_node.node_get_val("pple".as_bytes()), Some(&1));
        let (bytes_used, child_node) = replacement_node.borrow().node_get_child("banana".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 1);
        assert_eq!(child_node.node_get_val("anana".as_bytes()), Some(&2));
        let (bytes_used, child_node) = replacement_node.borrow().node_get_child("carrot".as_bytes()).unwrap();
        let child_node = child_node.borrow();
        assert_eq!(bytes_used, 1);
        assert_eq!(child_node.node_get_val("arrot".as_bytes()), Some(&3));
    }

    #[test]
    fn test_line_list_join_1_single_values_no_conflict() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("apple".as_bytes(), 0).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("banana".as_bytes(), 1).unwrap_or_else(|_| panic!());

        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));
        assert_eq!(join_list_node.node_get_val("apple".as_bytes()), Some(&0));
        assert_eq!(join_list_node.node_get_val("banana".as_bytes()), Some(&1));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));
        assert!(!join_list_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_2_single_values_joined() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("apple".as_bytes(), 42).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("apple".as_bytes(), 24).unwrap_or_else(|_| panic!());

        //u64's default impl of Lattice::join just takes the value from self
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));
        assert_eq!(join_list_node.node_get_val("apple".as_bytes()), Some(&42));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        assert!(!join_list_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_3_single_values_prefix_joined() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("apple".as_bytes(), 42).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("apricot".as_bytes(), 24).unwrap_or_else(|_| panic!());
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));

        let (remaining_key, child_node, _) = get_recursive("apple".as_bytes(), join_list_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&42));

        let (remaining_key, child_node, _) = get_recursive("apricot".as_bytes(), join_list_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&24));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        assert!(!join_list_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_4_bytes() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());
        a.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        b.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());

        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));
        assert_eq!(join_list_node.node_get_val("0".as_bytes()), Some(&0));
        assert_eq!(join_list_node.node_get_val("1".as_bytes()), Some(&1));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        assert!(!join_list_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_5_bytes() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());
        a.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());
        b.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&a));
        debug_assert!(validate_node(&b));

        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        debug_assert!(validate_node(join_list_node));
        assert_eq!(join_list_node.node_get_val("0".as_bytes()), Some(&0));
        assert_eq!(join_list_node.node_get_val("1".as_bytes()), Some(&1));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let join_list_node = joined_result.as_ref().map(|joined| joined.borrow().as_tagged().as_list().unwrap()).unwrap([&a, &b]);
        assert!(!join_list_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_6_bytes() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());
        a.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("2".as_bytes(), 2).unwrap_or_else(|_| panic!());
        b.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());

        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);
        assert_eq!(joined_node.node_get_val("0".as_bytes()), Some(&0));
        assert_eq!(joined_node.node_get_val("1".as_bytes()), Some(&1));
        assert_eq!(joined_node.node_get_val("2".as_bytes()), Some(&2));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);
        assert!(!joined_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_7_bytes() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("0".as_bytes(), 0).unwrap_or_else(|_| panic!());
        a.node_set_val("1".as_bytes(), 1).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("2".as_bytes(), 2).unwrap_or_else(|_| panic!());
        b.node_set_val("3".as_bytes(), 3).unwrap_or_else(|_| panic!());

        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);
        assert_eq!(joined_node.node_get_val("0".as_bytes()), Some(&0));
        assert_eq!(joined_node.node_get_val("1".as_bytes()), Some(&1));
        assert_eq!(joined_node.node_get_val("2".as_bytes()), Some(&2));
        assert_eq!(joined_node.node_get_val("3".as_bytes()), Some(&3));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);
        assert!(!joined_node.node_is_empty());
    }

    #[test]
    fn test_line_list_join_8_common_byte_prefix() {
        let mut a = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        a.node_set_val("0a".as_bytes(), 0).unwrap_or_else(|_| panic!());
        a.node_set_val("1a".as_bytes(), 1).unwrap_or_else(|_| panic!());
        let mut b = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        b.node_set_val("1b".as_bytes(), 1).unwrap_or_else(|_| panic!());
        b.node_set_val("2b".as_bytes(), 2).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&a));
        debug_assert!(validate_node(&b));

        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);

        let (remaining_key, child_node, _) = get_recursive("0a".as_bytes(), joined_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&0));

        let (remaining_key, child_node, _) = get_recursive("1a".as_bytes(), joined_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&1));

        let (remaining_key, child_node, _) = get_recursive("1b".as_bytes(), joined_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&1));

        let (remaining_key, child_node, _) = get_recursive("2b".as_bytes(), joined_node);
        assert_eq!(child_node.node_get_val(remaining_key), Some(&2));

        //re-run join, just to make sure the source maps didn't get modified
        let joined_result = a.pjoin_dyn(&b);
        let joined_node = joined_result.as_ref().map(|joined| joined.borrow()).unwrap([&a as &dyn TrieNode<_, GlobalAlloc>, &b]);
        assert!(!joined_node.node_is_empty());
    }

    #[test]
    fn test_list_node_child_count_at_key() {
        // k0="h", k1="hi", key="", result = 1
        // k0="h", k1="hi", key="h", result = 1
        // k0="h", k1="hi", key="hi", result = 0, because "hi" must be a value, otherwise the node would have advanced to the next node
        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"h", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"hi", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.count_branches(b"h"), 1);
        assert_eq!(node.count_branches(b""), 1);
        assert_eq!(node.count_branches(b"hi"), 0);

        // k0="ahoy", k1="howdy", key="h", result = 1
        // k0="ahoy", k1="howdy", key="", result = 2
        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"ahoy", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"howdy", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.count_branches(b"h"), 1);
        assert_eq!(node.count_branches(b""), 2);
    }

    #[test]
    fn test_line_list_siblings_and_children() {

        //Test two separate keys
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val(b"albatross", 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val(b"brown-winged whistling thrush (Myophonus castaneus)", 42).map_err(|_| 0), Ok((None, true)));
        debug_assert!(validate_node(&new_node));

        assert_eq!(new_node.count_branches(b""), 2);
        assert_eq!(new_node.count_branches(b"a"), 1);
        assert_eq!(new_node.count_branches(b"alb"), 1);
        assert_eq!(new_node.count_branches(b"albatross"), 0);
        assert_eq!(new_node.get_sibling_of_child(b"albatross", true).0, None);
        assert_eq!(new_node.get_sibling_of_child(b"brown-winged whistling thrush", true).0, None);
        assert_eq!(new_node.get_sibling_of_child(b"a", true).0, Some(b'b'));
        assert_eq!(new_node.get_sibling_of_child(b"b", true).0, None);
        assert_eq!(new_node.get_sibling_of_child(b"b", false).0, Some(b'a'));
        assert_eq!(new_node.get_sibling_of_child(b"a", false).0, None);

        //This leads to a node that holds both keys, although one is semantically a prefix to the other
        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val(b"a", 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val(b"albatross", 24).map_err(|_| 0), Ok((None, false)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.count_branches(b""), 1);
        assert_eq!(new_node.count_branches(b"a"), 1);
        assert_eq!(new_node.count_branches(b"al"), 1);
        assert_eq!(new_node.count_branches(b"albatross"), 0);
        assert_eq!(new_node.get_sibling_of_child(b"albatross", true).0, None);
        assert_eq!(new_node.get_sibling_of_child(b"a", true).0, None);
        assert_eq!(new_node.get_sibling_of_child(b"al", true).0, None);

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("al".as_bytes(), 24).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("albatross".as_bytes(), 42).map_err(|_| 0), Ok((None, true)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.node_set_val("a".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.count_branches(b""), 1);
        // assert_eq!(new_node.child_count_at_key(b"a"), 1); NOTE: This looks like it should be return 1, but this is not a valid argument for `child_count_at_key`
        assert_eq!(new_node.get_sibling_of_child(b"a", true).0, None);

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("albatross".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("al".as_bytes(), 24).map_err(|_| 0), Ok((None, true)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.node_set_val("a".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.count_branches(b""), 1);
        assert_eq!(new_node.get_sibling_of_child(b"a", true).0, None);

        let mut new_node = LineListNode::<usize, GlobalAlloc>::new_in(global_alloc());
        assert_eq!(new_node.node_set_val("albatross".as_bytes(), 42).map_err(|_| 0), Ok((None, false)));
        assert_eq!(new_node.node_set_val("a".as_bytes(), 24).map_err(|_| 0), Ok((None, false)));
        debug_assert!(validate_node(&new_node));
        assert_eq!(new_node.count_branches(b""), 1);
        assert_eq!(new_node.get_sibling_of_child(b"a", true).0, None);
    }

    #[test]
    fn test_line_list_sort_order() {
        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"aaa", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"bbb", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.nth_child_from_key(b"", 0).0, Some(b'a'));
        assert_eq!(node.nth_child_from_key(b"", 1).0, Some(b'b'));
        assert_eq!(node.first_child_from_key(b"").0, Some(&b"aaa"[..]));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"bbb", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"aaa", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.nth_child_from_key(b"", 0).0, Some(b'a'));
        assert_eq!(node.nth_child_from_key(b"", 1).0, Some(b'b'));
        assert_eq!(node.first_child_from_key(b"").0, Some(&b"aaa"[..]));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"a", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"ab", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.nth_child_from_key(b"", 0).0, Some(b'a'));
        assert_eq!(node.nth_child_from_key(b"", 1).0, None);
        assert_eq!(node.nth_child_from_key(b"a", 0).0, Some(b'b'));
        assert_eq!(node.first_child_from_key(b"").0, Some(&b"a"[..]));
        assert_eq!(node.first_child_from_key(b"a").0, Some(&b"b"[..]));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"ab", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"a", 1).unwrap_or_else(|_| panic!());
        debug_assert!(validate_node(&node));
        assert_eq!(node.nth_child_from_key(b"", 0).0, Some(b'a'));
        assert_eq!(node.nth_child_from_key(b"", 1).0, None);
        assert_eq!(node.nth_child_from_key(b"a", 0).0, Some(b'b'));
        assert_eq!(node.first_child_from_key(b"").0, Some(&b"a"[..]));
        assert_eq!(node.first_child_from_key(b"a").0, Some(&b"b"[..]));
    }

    #[test]
    fn test_line_list_clone_at_key() {
        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"apple", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"almond", 1).unwrap_or_else(|_| panic!());
        node.node_set_val(b"a", 2).unwrap_or_else(|_| panic!());
        let inner_node = node.get_node_at_key(b"a");
        assert_eq!(inner_node.borrow().node_get_val(b"pple"), Some(&0));
        assert_eq!(inner_node.borrow().node_get_val(b"lmond"), Some(&1));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"apple", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"apricot", 1).unwrap_or_else(|_| panic!());
        let inner_node = node.get_node_at_key(b"a");
        assert!(inner_node.borrow().node_get_child(b"p").is_some());
        let inner_node = node.get_node_at_key(b"ap");
        assert_eq!(inner_node.borrow().node_get_val(b"ple"), Some(&0));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"apple", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"a", 1).unwrap_or_else(|_| panic!());
        let inner_node = node.get_node_at_key(b"a");
        assert_eq!(inner_node.borrow().node_get_val(b"pple"), Some(&0));

        let mut node = LineListNode::<u64, GlobalAlloc>::new_in(global_alloc());
        node.node_set_val(b"apple", 0).unwrap_or_else(|_| panic!());
        node.node_set_val(b"banana", 1).unwrap_or_else(|_| panic!());
        let inner_node = node.get_node_at_key(b"ap");
        assert_eq!(inner_node.borrow().node_get_val(b"ple"), Some(&0));
        let inner_node = node.get_node_at_key(b"b");
        assert_eq!(inner_node.borrow().node_get_val(b"anana"), Some(&1));
    }

}

//GOAT, make an is_shared() zipper method, with all relevant caveats in the documentation
//
//GOAT, tests to make sure the right status codes are returned from all algebraic ops
//
//GOAT, merge wrappers for lattice impls on primitives
//
//GOAT, remove garbage lattice impls
//
//GOAT, rename BytesTrieMap to PathMap, consider other renames, marked by GOATs
//GOAT Catamorphism names:  https://github.com/Adam-Vandervorst/PathMap/pull/8#discussion_r2004745719
//
//GOAT, document how path existence can't be used to confirm the existence of a value, only the non-existence
//  and document the meaning of path existence more generally.
//GOAT, consider exposing an explicit prune method.  Possibly also consider exposing a "create_path" method.
//  SEE "PATH EXISTS DISCUSSION" below
//
//GOAT, consider adding a "prune" flag to methods that might remove values
//


// PATH EXISTS DISCUSSION
// Ok... Fork 1 is about paths, and specifically what information about values you can get from whether or not a path exists.  In the current code, the *nonexistence* of a path guarantees no value is below that point (how could there be one?) but the *existence* of a path does **not** guarantee a value is.
// Earlier drafts of PathMap (about 3 months ago) we were upholding that property that all paths led to values.  But I realized this property is impossible to uphold with the a multi write-zipper implementation.
// Bottom line, with the current set of guarantees, you can't use `path_exists` to conclude that there are zippers above you.  You will have to call `to_next_value` to search downwards.
// Looking forwards, I think I may add explicit methods like `ascend_prune` that ascends the zipper upwards from an empty leaf, pruning as it goes, and `descend_create` to do the opposite.  (although I'm a little on the fence about how descend_create would actually be useful.)  Maybe it might make sense to implement versions of these methods that don't move the zipper focus.
// But if we tweak the zipper contract so that paths are explicitly managed, just like values, and document the behavior of every operation with respect to paths, then the existence of a path might become a reliable signal.
// However, that brings up another question: Do you *want* to be pruning the path each time?  Consider a loop where a zipper is acquired, dropped, acquired, dropped, etc.  If each acquisition means creating the path, and each drop means pruning it, that is a lot of wasted work.  On the other hand, just setting and clearing a value is a lot cheaper.
// Anyway, let me know your thoughts.

//GOAT, Put the old cursor behind a feature flag, and prepare it for removal

//GOAT, move range into "utils" module, and integrate efficient implementation

//GOAT, write up plan for generalization of caching val_count

//GOAT, fix the issue with the iterators and the tracker, and the iterators and the root values

//GOAT, look at the `move_to(path)` zipper movement API, to avoid ascending too far

//GOAT, implement IntoIterator on PathMap

//GOAT, the following tests appear to leak memory, when running under miri:
// * test write_zipper::tests::write_zipper_test_zipper_conversion ... ok

//GOAT, Paths in caching Cata:  https://github.com/Adam-Vandervorst/PathMap/pull/8#discussion_r2004828957