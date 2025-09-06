use std::borrow::Cow;
use crate::utils::ByteMask;
use crate::zipper::*;

enum PrefixPos {
    Prefix { valid: usize },
    PrefixOff { valid: usize, invalid: usize },
    Source,
}

impl PrefixPos {
    // #[inline]
    // fn is_prefix(&self) -> bool {
    //     matches!(self, PrefixPos::Prefix {..})
    // }
    #[inline]
    fn is_invalid(&self) -> bool {
        matches!(self, PrefixPos::PrefixOff {..})
    }
    #[inline]
    fn is_source(&self) -> bool {
        matches!(self, PrefixPos::Source)
    }
    #[inline]
    fn prefixed_depth(&self) -> Option<usize> {
        match self {
            PrefixPos::Prefix { valid } => Some(*valid),
            PrefixPos::PrefixOff { valid, invalid } => Some(*valid + *invalid),
            PrefixPos::Source => None,
        }
    }
}

/// A [Zipper] type that wrapps another `Zipper`, and allows an arbitrary path to prepend the
/// wrapper's zipper's space
///
/// ```
/// use pathmap::{PathMap, zipper::*};
///
/// let map: PathMap<()> = [(b"A", ()), (b"B", ())].into_iter().collect();
/// let mut rz = PrefixZipper::new(b"origin.prefix.", map.read_zipper());
/// rz.set_root_prefix_path(b"origin.").unwrap();
///
/// rz.descend_to(b"prefix.A");
/// assert_eq!(rz.path_exists(), true);
/// assert_eq!(rz.origin_path(), b"origin.prefix.A");
/// assert_eq!(rz.path(), b"prefix.A");
/// assert_eq!(rz.root_prefix_path(), b"origin.");
/// ```
pub struct PrefixZipper<'prefix, Z> {
    path: Vec<u8>,
    source: Z,
    prefix: Cow<'prefix, [u8]>,
    origin_depth: usize,
    position: PrefixPos,
}

impl<'prefix, Z>  PrefixZipper<'prefix, Z>
    where
        Z: ZipperMoving
{
    /// Creates a new `PrefixZipper` wrapping the supplied `source` zipper and prepending the
    /// supplied `prefix`
    pub fn new<P>(prefix: P, mut source: Z) -> Self
        where P: Into<Cow<'prefix, [u8]>>
    {
        let prefix = prefix.into();
        source.reset();
        let position = if prefix.is_empty() {
            PrefixPos::Source
        } else {
            PrefixPos::Prefix { valid: 0 }
        };
        Self {
            path: Vec::new(),
            source,
            prefix,
            origin_depth: 0,
            position,
        }
    }

    /// Sets the portion of the zipper's `prefix` to treat as the [`root_prefix_path`](ZipperAbsolutePath::root_prefix_path)
    ///
    /// The remaining portion of the `prefix` will be part of the [`path`](ZipperMoving::path).
    /// This method resets the zipper, and typically it is called immediately after creating the `PrefixZipper`.
    pub fn set_root_prefix_path(&mut self, root_prefix_path: &[u8]) -> Result<(), &'static str> {
        if !self.prefix.starts_with(root_prefix_path) {
            return Err("zipper's prefix must begin with root_prefix_path");
        }
        self.origin_depth = root_prefix_path.len();
        self.reset();
        Ok(())
    }
    fn set_valid(&mut self, valid: usize) {
        debug_assert!(valid <= self.prefix.len(), "valid prefix can't be outside prefix");
        self.position = if valid == self.prefix.len() - self.origin_depth {
            PrefixPos::Source
        } else {
            PrefixPos::Prefix { valid }
        };
    }

    fn ascend_n(&mut self, mut steps: usize) -> Result<(), usize> {
        if let PrefixPos::PrefixOff { valid, mut invalid } = self.position {
            if invalid > steps {
                invalid -= steps;
                self.position = PrefixPos::PrefixOff { valid, invalid };
                return Ok(());
            }
            steps -= invalid;
            self.set_valid(valid.saturating_sub(steps));
            return if let Some(remaining) = steps.checked_sub(valid) {
                Err(remaining)
            } else {
                Ok(())
            };
        }
        if self.position.is_source() {
            // let Err(remaining) = self.source.ascend(steps) else {
            //     return Ok(());
            // };
            let len_before = self.source.path().len();
            if self.source.ascend(steps) {
                return Ok(())
            }
            let len_after = self.source.path().len();
            steps -= len_before - len_after;
            self.position = PrefixPos::Prefix { valid: self.prefix.len() - self.origin_depth };
            // Intermediate state: self.position points one off
        }
        if let PrefixPos::Prefix { valid } = self.position {
            self.set_valid(valid.saturating_sub(steps));
            return if let Some(remaining) = steps.checked_sub(valid) {
                Err(remaining)
            } else {
                Ok(())
            };
        }
        Err(steps)
    }
    fn ascend_until_n<const VAL: bool>(&mut self) -> Option<usize> {
        if self.at_root() {
            return None;
        }
        let mut ascended = 0;
        if self.position.is_source() {
            // if let Some(moved) = self.source.ascend_until() {
            //     return Some(moved);
            // }
            let len_before = self.source.path().len();
            let was_good = if VAL {
                self.source.ascend_until()
            } else {
                self.source.ascend_until_branch()
            };
            if was_good && ((VAL && self.source.is_val()) || self.source.child_count() > 1) {
                let len_after = self.source.path().len();
                return Some(len_before - len_after);
            }
            ascended += len_before;
            let valid = self.prefix.len() - self.origin_depth;
            self.position = PrefixPos::Prefix { valid };
        }
        ascended += self.position.prefixed_depth()
            .expect("we should no longer pointe at source at this point");
        self.set_valid(0);
        Some(ascended)
    }
}

impl<'prefix, Z> ZipperConcretePriv for PrefixZipper<'prefix, Z>
    where
        Z: ZipperConcretePriv
{
    fn shared_node_id(&self) -> Option<u64> {
        match self.position {
            PrefixPos::Source => self.source.shared_node_id(),
            _ => None,
        }
    }
}

impl<'prefix, Z> ZipperConcrete for PrefixZipper<'prefix, Z>
    where
        Z: ZipperConcrete
{
    fn is_shared(&self) -> bool {
        match self.position {
            PrefixPos::Source => self.source.is_shared(),
            _ => false,
        }
    }
}

impl<'prefix, Z, V> ZipperValues<V> for PrefixZipper<'prefix, Z>
    where
        Z: ZipperValues<V>
{
    fn val(&self) -> Option<&V> {
        if !self.position.is_source() {
            return None;
        }
        self.source.val()
    }
}

impl<'prefix, 'source, Z, V> ZipperReadOnlyValues<'source, V>
    for PrefixZipper<'prefix, Z>
    where
        Z: ZipperReadOnlyValues<'source, V>
{
    fn get_val(&self) -> Option<&'source V> {
        if !self.position.is_source() {
            return None;
        }
        self.source.get_val()
    }
}

impl<'prefix, 'source, Z, V> ZipperReadOnlyConditionalValues<'source, V>
    for PrefixZipper<'prefix, Z>
    where
        Z: ZipperReadOnlyConditionalValues<'source, V>
{
    type WitnessT = Z::WitnessT;
    fn witness<'w>(&self) -> Self::WitnessT { self.source.witness() }
    fn get_val_with_witness<'w>(&self, witness: &'w Self::WitnessT) -> Option<&'w V> where 'source: 'w {
        if !self.position.is_source() {
            return None;
        }
        self.source.get_val_with_witness(witness)
    }
}

impl<'prefix, Z> ZipperPathBuffer for PrefixZipper<'prefix, Z>
    where Z: ZipperMoving
{
    unsafe fn origin_path_assert_len(&self, len: usize) -> &[u8] {
        assert!(self.path.capacity() >= len);
        unsafe{ core::slice::from_raw_parts(self.path.as_ptr(), len) }
    }
    fn prepare_buffers(&mut self) {}
    fn reserve_buffers(&mut self, path_len: usize, _stack_depth: usize) {
        self.path.reserve(path_len);
    }
}

impl<'prefix, Z> Zipper for PrefixZipper<'prefix, Z>
    where
        Z: Zipper
{
    fn path_exists(&self) -> bool {
        match self.position {
            PrefixPos::Prefix {..} => true,
            PrefixPos::PrefixOff {..} => false,
            PrefixPos::Source => self.source.path_exists(),
        }
    }
    fn is_val(&self) -> bool {
        match self.position {
            PrefixPos::Source => self.source.is_val(),
            _ => false,
        }
    }
    fn child_count(&self) -> usize {
        match self.position {
            PrefixPos::Prefix {..} => 1,
            PrefixPos::PrefixOff {..} => 0,
            PrefixPos::Source => self.source.child_count(),
        }
    }
    fn child_mask(&self) -> ByteMask {
        match self.position {
            PrefixPos::Prefix { valid } => {
                let byte = self.prefix[self.origin_depth + valid];
                ByteMask::from(byte)
            },
            PrefixPos::PrefixOff {..} => ByteMask::EMPTY,
            PrefixPos::Source => self.source.child_mask(),
        }
    }
}

impl<'prefix, Z> ZipperMoving for PrefixZipper<'prefix, Z>
    where
        Z: ZipperMoving
{
    fn at_root(&self) -> bool {
        match self.position {
            PrefixPos::Prefix { valid } => valid == 0,
            PrefixPos::PrefixOff {..} => false,
            PrefixPos::Source => self.prefix.len() <= self.origin_depth && self.source.at_root(),
        }
    }

    fn reset(&mut self) {
        self.path.clear();
        self.path.extend_from_slice(&self.prefix[..self.origin_depth]);
        self.source.reset();
        self.set_valid(0);
    }

    #[inline]
    fn path(&self) -> &[u8] {
        &self.path[self.origin_depth..]
    }

    fn val_count(&self) -> usize {
        unimplemented!("method will probably get removed")
    }

    fn descend_to_existing<K: AsRef<[u8]>>(&mut self, patho: K) -> usize {
        if self.position.is_invalid() {
            return 0;
        }
        let mut descended = 0;
        let mut path = patho.as_ref();
        if let PrefixPos::Prefix { valid } = &self.position {
            let valid = *valid;
            let rest_prefix = &self.prefix[self.origin_depth + valid..];
            let overlap = crate::utils::find_prefix_overlap(rest_prefix, path);
            path = &path[overlap..];
            self.set_valid(valid + overlap);
            descended += overlap;
        }
        if self.position.is_source() {
            descended += self.source.descend_to_existing(path);
        }
        self.path.extend_from_slice(&patho.as_ref()[..descended]);
        descended
    }

    fn descend_to<K: AsRef<[u8]>>(&mut self, path: K) -> bool {
        let mut path = path.as_ref();
        let existing = self.descend_to_existing(path);
        path = &path[existing..];
        if path.is_empty() {
            return true;
        }
        self.path.extend_from_slice(&path);
        self.position = match self.position {
            PrefixPos::Prefix { valid } =>
                PrefixPos::PrefixOff { valid, invalid: path.len() },
            PrefixPos::PrefixOff { valid, invalid } =>
                PrefixPos::PrefixOff { valid, invalid: invalid + path.len() },
            PrefixPos::Source => {
                self.source.descend_to(path);
                PrefixPos::Source
            },
        };
        false
    }

    #[inline]
    fn descend_to_byte(&mut self, k: u8) -> bool {
        self.descend_to([k])
    }

    fn descend_indexed_byte(&mut self, child_idx: usize) -> bool {
        let mask = self.child_mask();
        let Some(byte) = mask.indexed_bit::<true>(child_idx) else {
            return false;
        };
        debug_assert!(self.descend_to_byte(byte));
        true
    }

    #[inline]
    fn descend_first_byte(&mut self) -> bool {
        self.descend_indexed_byte(0)
    }

    fn descend_until(&mut self) -> bool {
        if self.position.is_invalid() {
            return false;
        }
        if let Some(prefixed_depth) = self.position.prefixed_depth() {
            self.path.extend_from_slice(&self.prefix[self.origin_depth + prefixed_depth..]);
            self.position = PrefixPos::Source;
        }
        let len_before = self.source.path().len();
        if !self.source.descend_until() {
            return false;
        }
        let path = self.source.path();
        self.path.extend_from_slice(&path[len_before..]);
        true
    }

    #[inline]
    fn to_next_sibling_byte(&mut self) -> bool {
        if !self.position.is_source() {
            return false;
        }
        if !self.source.to_next_sibling_byte() {
            return false;
        }
        let byte = *self.source.path().last().unwrap();
        *self.path.last_mut().unwrap() = byte;
        true
    }

    #[inline]
    fn to_prev_sibling_byte(&mut self) -> bool {
        if !self.position.is_source() {
            return false;
        }
        if !self.source.to_prev_sibling_byte() {
            return false;
        }
        let byte = *self.source.path().last().unwrap();
        *self.path.last_mut().unwrap() = byte;
        true
    }
    fn ascend(&mut self, steps: usize) -> bool {
        let ascended = match self.ascend_n(steps) {
            Err(remaining) => steps - remaining,
            Ok(()) => steps,
        };
        self.path.truncate(self.path.len() - ascended);
        ascended > 0
    }
    #[inline]
    fn ascend_byte(&mut self) -> bool {
        self.ascend(1)
    }
    #[inline]
    fn ascend_until(&mut self) -> bool {
        let Some(ascended) = self.ascend_until_n::<true>() else {
            return false;
        };
        self.path.truncate(self.path.len() - ascended);
        true
    }
    #[inline]
    fn ascend_until_branch(&mut self) -> bool {
        let Some(ascended) = self.ascend_until_n::<false>() else {
            return false;
        };
        self.path.truncate(self.path.len() - ascended);
        true
    }
}

/// An interface for a [Zipper] to support accessing the full path buffer used to create the zipper
impl<'prefix, Z> ZipperAbsolutePath for PrefixZipper<'prefix, Z>
    where Z: ZipperAbsolutePath
{
    fn origin_path(&self) -> &[u8] {
        &self.path
    }
    fn root_prefix_path(&self) -> &[u8] {
        &self.path[..self.origin_depth]
    }
}

#[cfg(test)]
mod tests {
    use super::PrefixZipper;
    use crate::trie_map::PathMap;
    use crate::zipper::ZipperMoving;
    use crate::zipper::ZipperAbsolutePath;
    const PATHS1: &[(&[u8], u64)] = &[
        (b"0000", 0),
        (b"00000", 1),
        (b"00011", 2),
        (b"11111", 3),
        (b"11222", 4),
    ];
    const PATHS2: &[(&[u8], u64)] = &[
        (b"000", 0),
        (b"00000", 0),
        (b"00111", 1),
    ];

    #[test]
    fn test_prefix_zipper1() {
        let map = PathMap::from_iter(PATHS1.iter().map(|&x| x));
        let mut rz = PrefixZipper::new(b"prefix", map.read_zipper());
        rz.set_root_prefix_path(b"pre").unwrap();
        assert_eq!(rz.descend_to_existing(b"fix00000"), 8);
        assert_eq!(rz.ascend_until(), true);
        assert_eq!(rz.path(), b"fix0000");
        assert_eq!(rz.origin_path(), b"prefix0000");
        assert_eq!(rz.descend_to_existing(b"0"), 1);
        assert_eq!(rz.ascend_until_branch(), true);
        assert_eq!(rz.path(), b"fix000");
        assert_eq!(rz.ascend_until_branch(), true);
        assert_eq!(rz.path(), b"fix");
        assert_eq!(rz.ascend_until_branch(), true);
        assert_eq!(rz.path(), b"");
        assert_eq!(rz.origin_path(), b"pre");
        assert_eq!(rz.ascend_until_branch(), false);
    }

    #[test]
    fn test_prefix_zipper2() {
        let map = PathMap::from_iter(PATHS2.iter().map(|&x| x));
        let mut rz = PrefixZipper::new(b"prefix", map.read_zipper());
        rz.set_root_prefix_path(b"pre").unwrap();
        assert_eq!(rz.descend_to_existing(b"fix00000"), 8);
        assert_eq!(rz.ascend_until(), true);
        assert_eq!(rz.path(), b"fix000");
        assert_eq!(rz.origin_path(), b"prefix000");
        assert_eq!(rz.ascend_until(), true);
        assert_eq!(rz.path(), b"fix00");
        assert_eq!(rz.ascend_until(), true);
        assert_eq!(rz.path(), b"");
        assert_eq!(rz.ascend_until(), false);
        assert_eq!(rz.descend_to_existing(b"fix00000"), 8);
        assert_eq!(rz.ascend_until_branch(), true);
        assert_eq!(rz.path(), b"fix00");
        assert_eq!(rz.ascend_until_branch(), true);
        assert_eq!(rz.path(), b"");
        assert_eq!(rz.origin_path(), b"pre");
        assert_eq!(rz.ascend_until_branch(), false);
    }
}