
#[cfg(doc)]
use crate::zipper::*;

/// Derive macro to implement *most* zipper traits on an enum designed to act as a polymorphic zipper
///
/// A polymorphic zipper is a zipper that can represent different underlying zipper kinds, and dispatch
/// to the appropriate type at runtime.  Similar in concept to an `&dyn` reference.
///
/// The `PolyZipper` macro implements the following traits, provided they are implemented on each of the enum variants:
/// * [`Zipper`]
/// * [`ZipperAbsolutePath`]
/// * [`ZipperConcrete`]
/// * [`ZipperIteration`]
/// * [`ZipperMoving`]
/// * [`ZipperPathBuffer`]
/// * [`ZipperReadOnlyConditionalIteration`]
/// * [`ZipperReadOnlyConditionalValues`]
/// * [`ZipperReadOnlyIteration`]
/// * [`ZipperReadOnlyValues`]
/// * [`ZipperValues`]
///
/// NOTE: This macro does not derive an impl for [`ZipperForking`]
/// because the mapping between child zipper types and the output type is not always straightforward.
/// Therefore it is recommended to implement `ZipperForking` yourself.
///
/// [`ZipperWriting`] and other write zipper trait are also not supported currently. That decision is
/// not fundamental and additional impls could be added in the future.
///
/// ## USAGE:
/// The generic parameter names: `'trie`, `'path`, `V`, and `A` have special meaning to
/// the traits that require them.  `V` must be specified as a generic type paremeter, even if
/// you intend to specify a default type.
///
/// ```
/// use pathmap::zipper::{PolyZipper, ReadZipperTracked, ReadZipperUntracked};
///
/// #[derive(PolyZipper)]
/// enum MyPolyZipper<'trie, 'path, V: Clone + Send + Sync + Unpin = ()> {
///     Tracked(ReadZipperTracked<'trie, 'path, V>),
///     Untracked(ReadZipperUntracked<'trie, 'path, V>),
/// }
/// ```
pub use pathmap_derive::PolyZipper;

#[cfg(test)]
mod tests {
    use crate as pathmap;
    use crate::PathMap;
    use crate::zipper::*;

    #[cfg(feature = "arena_compact")]
    use crate::arena_compact::{ACTMmapZipper, ACTVecZipper, ACTVec};

    #[cfg(not(feature = "arena_compact"))]
    #[derive(PolyZipper)]
    pub enum TestPolyZipper<'trie, V: Clone + Send + Sync + Unpin = ()> {
        /// Tracked PathMap read zipper
        PathMap(ReadZipperTracked<'trie, 'trie, V>),
        /// Unracked PathMap read zipper.
        /// The reason this exists is to allow forking the zipper.
        /// Forking a Tracked read zipper return an Untracked read zipper.
        PathMapU(ReadZipperUntracked<'trie, 'trie, V>),
    }

    #[cfg(feature = "arena_compact")]
    #[derive(PolyZipper)]
    pub enum TestPolyZipper<'trie, V: Clone + Send + Sync + Unpin = ()> {
        /// Tracked PathMap read zipper
        PathMap(ReadZipperTracked<'trie, 'trie, V>),
        /// Unracked PathMap read zipper.
        /// The reason this exists is to allow forking the zipper.
        /// Forking a Tracked read zipper return an Untracked read zipper.
        PathMapU(ReadZipperUntracked<'trie, 'trie, V>),
        /// Memory-mapped ACT.
        /// Prefix is necessary here such that MORK can see ACT under a specific path
        ACTMmapPrefix(PrefixZipper<'trie, ACTMmapZipper<'trie, V>>),
        /// `Vec<u8>` based ACT
        /// Prefix is necessary here such that MORK can see ACT under a specific path
        ACTVecPrefix(PrefixZipper<'trie, ACTVecZipper<'trie, V>>),
    }


    crate::zipper::zipper_moving_tests::zipper_moving_tests!(poly_zipper_pm,
        |keys: &[&[u8]]| {
            keys.iter().map(|k| (k, ())).collect::<PathMap<()>>()
        },
        |btm: &mut PathMap<()>, path: &[u8]| -> _ {
            TestPolyZipper::PathMapU(btm.read_zipper_at_path(path))
        }
    );

    crate::zipper::zipper_iteration_tests::zipper_iteration_tests!(poly_zipper_pm,
        |keys: &[&[u8]]| {
            keys.iter().map(|k| (k, ())).collect::<PathMap<()>>()
        },
        |btm: &mut PathMap<()>, path: &[u8]| -> _ {
            TestPolyZipper::PathMapU(btm.read_zipper_at_path(path))
        }
    );

    #[cfg(feature = "arena_compact")]
    crate::zipper::zipper_moving_tests::zipper_moving_tests!(poly_zipper_act,
        |keys: &[&[u8]]| {
            let btm = keys.iter().map(|k| (k, ())).collect::<PathMap<()>>();
            ACTVec::from_zipper(btm.read_zipper(), |()| 0)
        },
        |act: &mut ACTVec, path: &[u8]| -> _ {
            TestPolyZipper::ACTVecPrefix(PrefixZipper::new(&[], act.read_zipper_at_path(path)))
        }
    );

    #[cfg(feature = "arena_compact")]
    crate::zipper::zipper_iteration_tests::zipper_iteration_tests!(poly_zipper_act,
        |keys: &[&[u8]]| {
            let btm = keys.iter().map(|k| (k, ())).collect::<PathMap<()>>();
            ACTVec::from_zipper(btm.read_zipper(), |()| 0)
        },
        |act: &mut ACTVec, path: &[u8]| -> _ {
            TestPolyZipper::ACTVecPrefix(PrefixZipper::new(&[], act.read_zipper_at_path(path)))
        }
    );
}
