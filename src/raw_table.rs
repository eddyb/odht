//! This module implements the actual hash table logic. It works solely with
//! byte arrays and does not know anything about the unencoded key and value
//! types.
//!
//! The implementation makes sure that the encoded data contains no padding
//! bytes (which makes it deterministic) and nothing in it requires any specific
//! alignment.
//!
//! Many functions in this module are marked as `#[inline]`. This is allows
//! LLVM to retain the information about byte array sizes, even though they are
//! converted to slices (of unknown size) from time to time.

use crate::swisstable_group_query::{GroupQuery, GROUP_SIZE};
use crate::{error::Error, HashFn};
use std::convert::TryInto;
use std::{fmt, marker::PhantomData, mem::size_of};

/// Values of this type represent key-value pairs *as they are stored on-disk*.
/// `#[repr(C)]` makes sure we have deterministic field order and the fields
/// being byte arrays makes sure that there are no padding bytes, alignment is
/// not restricted, and the data is endian-independent.
///
/// It is a strict requirement that any `&[Entry<K, V>]` can be transmuted
/// into a `&[u8]` and back, regardless of whether the byte array has been
/// moved in the meantime.
#[repr(C)]
#[derive(PartialEq, Eq, Default, Clone, Copy, Debug)]
pub(crate) struct Entry<K: ByteArray, V: ByteArray> {
    pub key: K,
    pub value: V,
}

impl<K: ByteArray, V: ByteArray> Entry<K, V> {
    #[inline]
    fn new(key: K, value: V) -> Entry<K, V> {
        Entry { key, value }
    }
}

impl<'a, K: ByteArray, V: ByteArray, H: HashFn> fmt::Debug for RawTable<'a, K, V, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<debug no implemented>")
    }
}

pub(crate) type EntryMetadata = u8;

type HashValue = u32;

#[inline]
fn h1(h: HashValue) -> usize {
    h as usize
}

#[inline]
fn h2(h: HashValue) -> u8 {
    const SHIFT: usize = size_of::<HashValue>() * 8 - 7;
    (h >> SHIFT) as u8
}

struct PropeSeq {
    index: usize,
    stride: usize,
}

impl PropeSeq {
    #[inline]
    fn new(h1: usize, mask: usize) -> PropeSeq {
        PropeSeq {
            index: h1 & mask,
            stride: 0,
        }
    }

    #[inline]
    fn advance(&mut self, mask: usize) {
        self.stride += GROUP_SIZE;
        self.index += self.stride;
        self.index &= mask;
    }
}

#[inline]
fn group_starting_at<'a>(control_bytes: &'a [u8], index: usize) -> &'a [u8; GROUP_SIZE] {
    debug_assert!(index < control_bytes.len() - GROUP_SIZE);
    unsafe {
        std::slice::from_raw_parts(control_bytes.as_ptr().offset(index as isize), GROUP_SIZE)
            .try_into()
            .unwrap()
    }
}

#[inline]
fn is_empty_or_deleted(control_byte: u8) -> bool {
    (control_byte & EMPTY_CONTROL_BYTE) != 0
}

const EMPTY_CONTROL_BYTE: u8 = 0b1000_0000;

/// This type provides a readonly view of the given table data.
#[derive(PartialEq, Eq)]
pub(crate) struct RawTable<'a, K, V, H>
where
    K: ByteArray,
    V: ByteArray,
    H: HashFn,
{
    metadata: &'a [EntryMetadata],
    data: &'a [Entry<K, V>],
    _hash_fn: PhantomData<H>,
}

impl<'a, K, V, H> RawTable<'a, K, V, H>
where
    K: ByteArray,
    V: ByteArray,
    H: HashFn,
{
    #[inline]
    pub(crate) fn new(metadata: &'a [EntryMetadata], data: &'a [Entry<K, V>]) -> Self {
        // Make sure Entry<K, V> does not contain any padding bytes and can be
        // stored at arbitrary adresses.
        assert!(size_of::<Entry<K, V>>() == size_of::<K>() + size_of::<V>());
        assert!(std::mem::align_of::<Entry<K, V>>() == 1);

        debug_assert!(data.len().is_power_of_two());
        debug_assert!(metadata.len() == data.len() + GROUP_SIZE);

        Self {
            metadata,
            data,
            _hash_fn: PhantomData::default(),
        }
    }

    #[inline]
    pub(crate) fn find(&self, key: &K) -> Option<&V> {
        debug_assert!(self.data.len().is_power_of_two());
        debug_assert!(self.metadata.len() == self.data.len() + GROUP_SIZE);

        let mask = self.data.len() - 1;
        let hash = H::hash(key.as_slice());
        let mut ps = PropeSeq::new(h1(hash), mask);
        let h2 = h2(hash);

        loop {
            let mut group_query = GroupQuery::from(group_starting_at(self.metadata, ps.index), h2);

            for m in &mut group_query {
                let index = (ps.index + m) & mask;

                let entry = self.entry_at(index);

                if likely!(entry.key.equals(key)) {
                    return Some(&entry.value);
                }
            }

            if likely!(group_query.any_empty()) {
                return None;
            }

            ps.advance(mask);
        }
    }

    #[inline]
    pub(crate) fn iter(&'a self) -> RawIter<'a, K, V> {
        RawIter::new(self.metadata, self.data)
    }

    /// Check (for the first `entries_to_check` entries) if the computed and
    /// the stored hash value match.
    ///
    /// A mismatch is an indication that the table has been deserialized with
    /// the wrong hash function.
    pub(crate) fn sanity_check_hashes(&self, slots_to_check: usize) -> Result<(), Error> {
        let slots_to_check = std::cmp::min(slots_to_check, self.data.len());

        for i in 0..slots_to_check {
            let metadata = self.metadata[i];
            let entry = &self.data[i];

            if is_empty_or_deleted(metadata) {
                if !entry.key.is_all_zeros() || !entry.value.is_all_zeros() {
                    let message = format!("Found empty entry with non-zero contents at {}", i);

                    return Err(Error(message));
                }
            } else {
                let hash = H::hash(entry.key.as_slice());
                let h2 = h2(hash);

                if metadata != h2 {
                    let message = format!(
                        "Control byte does not match hash value for entry {}. Expected {}, found {}.",
                        i, h2, metadata
                    );

                    return Err(Error(message));
                }
            }
        }

        Ok(())
    }

    #[inline]
    fn entry_at(&self, index: usize) -> &Entry<K, V> {
        debug_assert!(index < self.data.len());
        unsafe { self.data.get_unchecked(index) }
    }
}

/// This type provides a mutable view of the given table data. It allows for
/// inserting new entries but does *not* allow for growing the table.
#[derive(PartialEq, Eq)]
pub(crate) struct RawTableMut<'a, K, V, H>
where
    K: ByteArray,
    V: ByteArray,
    H: HashFn,
{
    metadata: &'a mut [EntryMetadata],
    data: &'a mut [Entry<K, V>],
    _hash_fn: PhantomData<H>,
}

impl<'a, K, V, H> RawTableMut<'a, K, V, H>
where
    K: ByteArray,
    V: ByteArray,
    H: HashFn,
{
    #[inline]
    pub(crate) fn new(metadata: &'a mut [EntryMetadata], data: &'a mut [Entry<K, V>]) -> Self {
        // Make sure Entry<K, V> does not contain any padding bytes and can be
        // stored at arbitrary adresses.
        assert!(size_of::<Entry<K, V>>() == size_of::<K>() + size_of::<V>());
        assert!(std::mem::align_of::<Entry<K, V>>() == 1);

        debug_assert!(data.len().is_power_of_two());
        debug_assert_eq!(metadata.len(), data.len() + GROUP_SIZE);

        Self {
            metadata,
            data,
            _hash_fn: PhantomData::default(),
        }
    }

    /// Inserts the given key value pair into the hash table.
    ///
    /// WARNING: This method assumes that there is free space in the hash table
    ///          somewhere. If there isn't it will end up in an infinite loop.
    #[inline]
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        assert!(self.data.len().is_power_of_two());
        debug_assert!(self.metadata.len() == self.data.len() + GROUP_SIZE);

        let mask = self.data.len() - 1;
        let hash = H::hash(key.as_slice());

        let mut ps = PropeSeq::new(h1(hash), mask);
        let h2 = h2(hash);

        loop {
            let mut group_query = GroupQuery::from(group_starting_at(self.metadata, ps.index), h2);

            for m in &mut group_query {
                let index = (ps.index + m) & mask;

                let entry = entry_at_mut(self.data, index);

                if likely!(entry.key.equals(&key)) {
                    let ret = Some(entry.value);
                    entry.value = value;
                    return ret;
                }
            }

            if let Some(first_empty) = group_query.first_empty() {
                let index = (ps.index + first_empty) & mask;
                *entry_at_mut(self.data, index) = Entry::new(key, value);
                *metadata_at_mut(self.metadata, index) = h2;

                if index < GROUP_SIZE {
                    let first_mirror = self.data.len();
                    *metadata_at_mut(self.metadata, first_mirror + index) = h2;
                    debug_assert_eq!(
                        self.metadata[..GROUP_SIZE],
                        self.metadata[self.data.len()..]
                    );
                }

                return None;
            }

            ps.advance(mask);
        }
    }
}

#[inline]
fn entry_at_mut<K: ByteArray, V: ByteArray>(
    data: &mut [Entry<K, V>],
    index: usize,
) -> &mut Entry<K, V> {
    debug_assert!(index < data.len());
    unsafe { data.get_unchecked_mut(index) }
}

#[inline]
fn metadata_at_mut(metadata: &mut [EntryMetadata], index: usize) -> &mut EntryMetadata {
    debug_assert!(index < metadata.len());
    unsafe { metadata.get_unchecked_mut(index) }
}

impl<'a, K: ByteArray, V: ByteArray, H: HashFn> fmt::Debug for RawTableMut<'a, K, V, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let readonly = RawTable::<'_, K, V, H>::new(self.metadata, self.data);
        write!(f, "{:?}", readonly)
    }
}

pub(crate) struct RawIter<'a, K, V>
where
    K: ByteArray,
    V: ByteArray,
{
    metadata: &'a [EntryMetadata],
    data: &'a [Entry<K, V>],
    current_index: usize,
}

impl<'a, K, V> RawIter<'a, K, V>
where
    K: ByteArray,
    V: ByteArray,
{
    pub(crate) fn new(metadata: &'a [EntryMetadata], data: &'a [Entry<K, V>]) -> RawIter<'a, K, V> {
        debug_assert!(data.len().is_power_of_two());
        debug_assert!(metadata.len() == data.len() + 16);

        RawIter {
            metadata,
            data,
            current_index: 0,
        }
    }
}

impl<'a, K, V> Iterator for RawIter<'a, K, V>
where
    K: ByteArray,
    V: ByteArray,
{
    type Item = (EntryMetadata, &'a Entry<K, V>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current_index == self.data.len() {
                return None;
            }

            let index = self.current_index;

            self.current_index += 1;

            let entry_metadata = self.metadata[index];
            if !is_empty_or_deleted(entry_metadata) {
                return Some((entry_metadata, &self.data[index]));
            }
        }
    }
}

/// A trait that lets us abstract over different lengths of fixed size byte
/// arrays. Don't implement it for anything other than fixed size byte arrays!
pub trait ByteArray:
    Sized + Copy + Eq + Clone + PartialEq + Default + fmt::Debug + 'static
{
    fn as_slice(&self) -> &[u8];
    fn equals(&self, other: &Self) -> bool;
    fn is_all_zeros(&self) -> bool {
        self.as_slice().iter().all(|b| *b == 0)
    }
}

macro_rules! impl_byte_array {
    ($len:expr) => {
        impl ByteArray for [u8; $len] {
            #[inline(always)]
            fn as_slice(&self) -> &[u8] {
                &self[..]
            }

            // This custom implementation of comparing the fixed size arrays
            // seems make a big difference for performance (at least for
            // 16+ byte keys)
            #[inline]
            fn equals(&self, other: &Self) -> bool {
                use std::mem::size_of;
                // Most branches here are optimized away at compile time
                // because they depend on values known at compile time.

                const USIZE: usize = size_of::<usize>();

                // Special case a few cases we care about
                if size_of::<Self>() == USIZE {
                    return read_usize(&self[..], 0) == read_usize(&other[..], 0);
                }

                if size_of::<Self>() == USIZE * 2 {
                    return read_usize(&self[..], 0) == read_usize(&other[..], 0)
                        && read_usize(&self[..], 1) == read_usize(&other[..], 1);
                }

                if size_of::<Self>() == USIZE * 3 {
                    return read_usize(&self[..], 0) == read_usize(&other[..], 0)
                        && read_usize(&self[..], 1) == read_usize(&other[..], 1)
                        && read_usize(&self[..], 2) == read_usize(&other[..], 2);
                }

                if size_of::<Self>() == USIZE * 4 {
                    return read_usize(&self[..], 0) == read_usize(&other[..], 0)
                        && read_usize(&self[..], 1) == read_usize(&other[..], 1)
                        && read_usize(&self[..], 2) == read_usize(&other[..], 2)
                        && read_usize(&self[..], 3) == read_usize(&other[..], 3);
                }

                // fallback
                return &self[..] == &other[..];

                #[inline(always)]
                fn read_usize(bytes: &[u8], index: usize) -> usize {
                    use std::convert::TryInto;
                    const STRIDE: usize = size_of::<usize>();
                    usize::from_le_bytes(
                        bytes[STRIDE * index..STRIDE * (index + 1)]
                            .try_into()
                            .unwrap(),
                    )
                }
            }
        }
    };
}

impl_byte_array!(0);
impl_byte_array!(1);
impl_byte_array!(2);
impl_byte_array!(3);
impl_byte_array!(4);
impl_byte_array!(5);
impl_byte_array!(6);
impl_byte_array!(7);
impl_byte_array!(8);
impl_byte_array!(9);
impl_byte_array!(10);
impl_byte_array!(11);
impl_byte_array!(12);
impl_byte_array!(13);
impl_byte_array!(14);
impl_byte_array!(15);
impl_byte_array!(16);
impl_byte_array!(17);
impl_byte_array!(18);
impl_byte_array!(19);
impl_byte_array!(20);
impl_byte_array!(21);
impl_byte_array!(22);
impl_byte_array!(23);
impl_byte_array!(24);
impl_byte_array!(25);
impl_byte_array!(26);
impl_byte_array!(27);
impl_byte_array!(28);
impl_byte_array!(29);
impl_byte_array!(30);
impl_byte_array!(31);
impl_byte_array!(32);

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
    use crate::FxHashFn;

    fn make_table<
        I: Iterator<Item = (K, V)> + ExactSizeIterator,
        K: ByteArray,
        V: ByteArray,
        H: HashFn,
    >(
        xs: I,
    ) -> (Vec<EntryMetadata>, Vec<Entry<K, V>>) {
        let size = xs.size_hint().0.next_power_of_two();
        let mut data = vec![Entry::default(); size];
        let mut metadata = vec![255; size + 16];

        assert!(metadata.iter().all(|b| is_empty_or_deleted(*b)));

        {
            let mut table: RawTableMut<K, V, H> = RawTableMut {
                metadata: &mut metadata[..],
                data: &mut data[..],
                _hash_fn: Default::default(),
            };

            for (k, v) in xs {
                table.insert(k, v);
            }
        }

        (metadata, data)
    }

    #[test]
    fn stress() {
        let xs: Vec<[u8; 2]> = (0 ..= u16::MAX).map(|x| x.to_le_bytes()).collect();

        let (mut metadata, mut data) = make_table::<_, _, _, FxHashFn>(xs.iter().map(|x| (*x, *x)));

        {
            let table: RawTable<_, _, FxHashFn> = RawTable {
                metadata: &metadata[..],
                data: &data[..],
                _hash_fn: PhantomData::default(),
            };

            for x in xs.iter() {
                assert_eq!(table.find(x), Some(x));
            }
        }

        // overwrite all values
        {
            let mut table: RawTableMut<_, _, FxHashFn> = RawTableMut {
                metadata: &mut metadata[..],
                data: &mut data[..],
                _hash_fn: PhantomData::default(),
            };

            for (i, x) in xs.iter().enumerate() {
                assert_eq!(table.insert(*x, [i as u8, (i + 1) as u8]), Some(*x));
            }
        }

        // Check if we find the new expected values
        {
            let table: RawTable<_, _, FxHashFn> = RawTable {
                metadata: &metadata[..],
                data: &data[..],
                _hash_fn: PhantomData::default(),
            };

            for (i, x) in xs.iter().enumerate() {
                assert_eq!(table.find(x), Some(&[i as u8, (i + 1) as u8]));
            }
        }
    }
}
