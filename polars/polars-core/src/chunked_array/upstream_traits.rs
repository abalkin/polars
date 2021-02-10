//! Implementations of upstream traits for ChunkedArray<T>
use crate::chunked_array::builder::get_list_builder;
use crate::prelude::*;
use crate::utils::get_iter_capacity;
use crate::utils::NoNull;
use arrow::array::{BooleanArray, LargeStringArray, PrimitiveArray};
use polars_arrow::utils::TrustMyLength;
use rayon::iter::{FromParallelIterator, IntoParallelIterator};
use rayon::prelude::*;
use std::borrow::{Borrow, Cow};
use std::collections::LinkedList;
use std::iter::FromIterator;
use std::marker::PhantomData;
use std::sync::Arc;

impl<T> Default for ChunkedArray<T> {
    fn default() -> Self {
        ChunkedArray {
            field: Arc::new(Field::new("default", DataType::Null)),
            chunks: Default::default(),
            chunk_id: Default::default(),
            phantom: PhantomData,
            categorical_map: None,
        }
    }
}

/// FromIterator trait

impl<T> FromIterator<Option<T::Native>> for ChunkedArray<T>
where
    T: PolarsPrimitiveType,
{
    fn from_iter<I: IntoIterator<Item = Option<T::Native>>>(iter: I) -> Self {
        let iter = iter.into_iter();

        let arr: PrimitiveArray<T> = match iter.size_hint() {
            (a, Some(b)) if a == b => {
                // 2021-02-07: ~40% faster than builder.
                // It is unsafe because we cannot be certain that the iterators length can be trusted.
                // For most iterators that report the same upper bound as lower bound it is, but still
                // somebody can create an iterator that incorrectly gives those bounds.
                // This will not lead to UB, but will panic.
                #[cfg(feature = "performant")]
                unsafe {
                    let arr = PrimitiveArray::from_trusted_len_iter(iter);
                    assert_eq!(arr.len(), a);
                    arr
                }
                #[cfg(not(feature = "performant"))]
                PrimitiveArray::from_iter(iter)
            }
            _ => {
                // 2021-02-07: ~1.5% slower than builder. Will still use this as it is more idiomatic and will
                // likely improve over time.
                PrimitiveArray::from_iter(iter)
            }
        };
        ChunkedArray::new_from_chunks("", vec![Arc::new(arr)])
    }
}

// NoNull is only a wrapper needed for specialization
impl<T> FromIterator<T::Native> for NoNull<ChunkedArray<T>>
where
    T: PolarsPrimitiveType,
{
    // We use AlignedVec because it is way faster than Arrows builder. We can do this because we
    // know we don't have null values.
    fn from_iter<I: IntoIterator<Item = T::Native>>(iter: I) -> Self {
        // 2021-02-07: aligned vec was ~2x faster than arrow collect.
        let iter = iter.into_iter();
        let mut av = AlignedVec::with_capacity_aligned(0);
        av.extend(iter);
        NoNull::new(ChunkedArray::new_from_aligned_vec("", av))
    }
}

impl FromIterator<Option<bool>> for ChunkedArray<BooleanType> {
    fn from_iter<I: IntoIterator<Item = Option<bool>>>(iter: I) -> Self {
        let arr = BooleanArray::from_iter(iter);
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

impl FromIterator<bool> for BooleanChunked {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        // 2021-02-07: this was ~70% faster than with the builder, even with the extra Option<T> added.
        let arr = BooleanArray::from_iter(iter.into_iter().map(Some));
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

impl FromIterator<bool> for NoNull<BooleanChunked> {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let ca = iter.into_iter().collect::<BooleanChunked>();
        NoNull::new(ca)
    }
}

// FromIterator for Utf8Chunked variants.

impl<Ptr> FromIterator<Option<Ptr>> for Utf8Chunked
where
    Ptr: AsRef<str>,
{
    fn from_iter<I: IntoIterator<Item = Option<Ptr>>>(iter: I) -> Self {
        // 2021-02-07: this was ~30% faster than with the builder.
        let arr = LargeStringArray::from_iter(iter);
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

/// Local AsRef<T> trait to circumvent the orphan rule.
pub trait PolarsAsRef<T: ?Sized>: AsRef<T> {}

impl PolarsAsRef<str> for String {}
impl PolarsAsRef<str> for &str {}
// &["foo", "bar"]
impl PolarsAsRef<str> for &&str {}
impl<'a> PolarsAsRef<str> for Cow<'a, str> {}

impl<Ptr> FromIterator<Ptr> for Utf8Chunked
where
    Ptr: PolarsAsRef<str>,
{
    fn from_iter<I: IntoIterator<Item = Ptr>>(iter: I) -> Self {
        let arr = LargeStringArray::from_iter_values(iter);
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

impl<Ptr> FromIterator<Ptr> for ListChunked
where
    Ptr: Borrow<Series>,
{
    fn from_iter<I: IntoIterator<Item = Ptr>>(iter: I) -> Self {
        let mut it = iter.into_iter();
        let capacity = get_iter_capacity(&it);

        // first take one to get the dtype. We panic if we have an empty iterator
        let v = it.next().unwrap();
        // We don't know the needed capacity. We arbitrarily choose an average of 5 elements per series.
        let mut builder = get_list_builder(v.borrow().dtype(), capacity * 5, capacity, "collected");

        builder.append_series(v.borrow());
        for s in it {
            builder.append_series(s.borrow());
        }
        builder.finish()
    }
}

impl<Ptr> FromIterator<Option<Ptr>> for ListChunked
where
    Ptr: Borrow<Series>,
{
    fn from_iter<I: IntoIterator<Item = Option<Ptr>>>(iter: I) -> Self {
        let mut it = iter.into_iter();
        let owned_v;
        let mut cnt = 0;

        loop {
            let opt_v = it.next();

            match opt_v {
                Some(opt_v) => match opt_v {
                    Some(val) => {
                        owned_v = val;
                        break;
                    }
                    None => cnt += 1,
                },
                // end of iterator
                None => {
                    // type is not known
                    panic!("Type of Series cannot be determined as they are all null")
                }
            }
        }
        let v = owned_v.borrow();
        let capacity = get_iter_capacity(&it);
        let mut builder = get_list_builder(v.dtype(), capacity * 5, capacity, "collected");

        // first fill all None's we encountered
        while cnt > 0 {
            builder.append_opt_series(None);
            cnt -= 1;
        }

        // now the first non None
        builder.append_series(&v);

        // now we have added all Nones, we can consume the rest of the iterator.
        for opt_s in it {
            match opt_s {
                Some(s) => builder.append_series(s.borrow()),
                None => builder.append_null(),
            }
        }

        builder.finish()
    }
}

/// FromParallelIterator trait
// Code taken from https://docs.rs/rayon/1.3.1/src/rayon/iter/extend.rs.html#356-366
fn vec_push<T>(mut vec: Vec<T>, elem: T) -> Vec<T> {
    vec.push(elem);
    vec
}

fn as_list<T>(item: T) -> LinkedList<T> {
    let mut list = LinkedList::new();
    list.push_back(item);
    list
}

fn list_append<T>(mut list1: LinkedList<T>, mut list2: LinkedList<T>) -> LinkedList<T> {
    list1.append(&mut list2);
    list1
}

fn collect_into_linked_list<I>(par_iter: I) -> LinkedList<Vec<I::Item>>
where
    I: IntoParallelIterator,
{
    par_iter
        .into_par_iter()
        .fold(Vec::new, vec_push)
        .map(as_list)
        .reduce(LinkedList::new, list_append)
}

fn get_capacity_from_par_results<T>(ll: &LinkedList<Vec<T>>) -> usize {
    ll.iter().map(|list| list.len()).sum()
}

impl<T> FromParallelIterator<T::Native> for NoNull<ChunkedArray<T>>
where
    T: PolarsPrimitiveType,
{
    fn from_par_iter<I: IntoParallelIterator<Item = T::Native>>(iter: I) -> Self {
        // Get linkedlist filled with different vec result from different threads
        let vectors = collect_into_linked_list(iter);
        let capacity: usize = get_capacity_from_par_results(&vectors);

        let iter = TrustMyLength::new(vectors.into_iter().flatten(), capacity).map(Some);
        let arr: PrimitiveArray<T> = unsafe { PrimitiveArray::from_trusted_len_iter(iter) };
        NoNull::new(ChunkedArray::new_from_chunks("", vec![Arc::new(arr)]))
    }
}

impl<T> FromParallelIterator<Option<T::Native>> for ChunkedArray<T>
where
    T: PolarsPrimitiveType,
{
    fn from_par_iter<I: IntoParallelIterator<Item = Option<T::Native>>>(iter: I) -> Self {
        // Get linkedlist filled with different vec result from different threads
        let vectors = collect_into_linked_list(iter);
        let capacity: usize = get_capacity_from_par_results(&vectors);

        let iter = TrustMyLength::new(vectors.into_iter().flatten(), capacity);
        let arr: PrimitiveArray<T> = unsafe { PrimitiveArray::from_trusted_len_iter(iter) };
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

impl FromParallelIterator<bool> for BooleanChunked {
    fn from_par_iter<I: IntoParallelIterator<Item = bool>>(iter: I) -> Self {
        let vectors = collect_into_linked_list(iter);

        let capacity: usize = get_capacity_from_par_results(&vectors);

        let mut builder = BooleanChunkedBuilder::new("", capacity);
        // Unpack all these results and append them single threaded
        vectors.iter().for_each(|vec| {
            for val in vec {
                builder.append_value(*val);
            }
        });

        builder.finish()
    }
}

impl<Ptr> FromParallelIterator<Ptr> for Utf8Chunked
where
    Ptr: PolarsAsRef<str> + Send + Sync,
{
    fn from_par_iter<I: IntoParallelIterator<Item = Ptr>>(iter: I) -> Self {
        let vectors = collect_into_linked_list(iter);
        let arr = LargeStringArray::from_iter_values(vectors.into_iter().flatten());
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

impl<Ptr> FromParallelIterator<Option<Ptr>> for Utf8Chunked
where
    Ptr: AsRef<str> + Send + Sync,
{
    fn from_par_iter<I: IntoParallelIterator<Item = Option<Ptr>>>(iter: I) -> Self {
        let vectors = collect_into_linked_list(iter);
        let arr = LargeStringArray::from_iter(vectors.into_iter().flatten());
        Self::new_from_chunks("", vec![Arc::new(arr)])
    }
}

/// From trait
impl<'a> From<&'a Utf8Chunked> for Vec<Option<&'a str>> {
    fn from(ca: &'a Utf8Chunked) -> Self {
        ca.into_iter().collect()
    }
}

impl From<Utf8Chunked> for Vec<Option<String>> {
    fn from(ca: Utf8Chunked) -> Self {
        ca.into_iter()
            .map(|opt| opt.map(|s| s.to_string()))
            .collect()
    }
}

impl<'a> From<&'a BooleanChunked> for Vec<Option<bool>> {
    fn from(ca: &'a BooleanChunked) -> Self {
        ca.into_iter().collect()
    }
}

impl From<BooleanChunked> for Vec<Option<bool>> {
    fn from(ca: BooleanChunked) -> Self {
        ca.into_iter().collect()
    }
}

impl<'a, T> From<&'a ChunkedArray<T>> for Vec<Option<T::Native>>
where
    T: PolarsNumericType,
{
    fn from(ca: &'a ChunkedArray<T>) -> Self {
        ca.into_iter().collect()
    }
}

#[cfg(test)]
mod test {
    use crate::prelude::*;

    #[test]
    fn test_collect_into_list() {
        let s1 = Series::new("", &[true, false, true]);
        let s2 = Series::new("", &[true, false, true]);

        let ll: ListChunked = [&s1, &s2].iter().copied().collect();
        assert_eq!(ll.len(), 2);
        assert_eq!(ll.null_count(), 0);
        let ll: ListChunked = [None, Some(s2)].iter().map(|opt| opt.as_ref()).collect();
        assert_eq!(ll.len(), 2);
        assert_eq!(ll.null_count(), 1);
    }
}
