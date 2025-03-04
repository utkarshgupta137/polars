use std::convert::TryFrom;
use std::fmt::Write;

use polars_arrow::kernels::list::sublist_get;
use polars_arrow::prelude::ValueSize;
use polars_core::chunked_array::builder::get_list_builder;
#[cfg(feature = "list_take")]
use polars_core::export::num::ToPrimitive;
#[cfg(feature = "list_take")]
use polars_core::export::num::{NumCast, Signed, Zero};
#[cfg(feature = "diff")]
use polars_core::series::ops::NullBehavior;
use polars_core::utils::{try_get_supertype, CustomIterTools};

use super::*;
use crate::chunked_array::list::min_max::{list_max_function, list_min_function};
use crate::prelude::list::sum_mean::{mean_list_numerical, sum_list_numerical};
use crate::series::ArgAgg;

pub(super) fn has_inner_nulls(ca: &ListChunked) -> bool {
    for arr in ca.downcast_iter() {
        if arr.values().null_count() > 0 {
            return true;
        }
    }
    false
}

fn cast_rhs(
    other: &mut [Series],
    inner_type: &DataType,
    dtype: &DataType,
    length: usize,
    allow_broadcast: bool,
) -> PolarsResult<()> {
    for s in other.iter_mut() {
        // make sure that inner types match before we coerce into list
        if !matches!(s.dtype(), DataType::List(_)) {
            *s = s.cast(inner_type)?
        }
        if !matches!(s.dtype(), DataType::List(_)) && s.dtype() == inner_type {
            // coerce to list JIT
            *s = s.reshape(&[-1, 1]).unwrap();
        }
        if s.dtype() != dtype {
            *s = s.cast(dtype).map_err(|e| {
                polars_err!(
                    SchemaMismatch:
                    "cannot concat `{}` into a list of `{}`: {}",
                    s.dtype(),
                    dtype,
                    e
                )
            })?;
        }

        if s.len() != length {
            polars_ensure!(
                s.len() == 1,
                ShapeMismatch: "series length {} does not match expected length of {}",
                s.len(), length
            );
            if allow_broadcast {
                // broadcast JIT
                *s = s.new_from_index(0, length)
            }
            // else do nothing
        }
    }
    Ok(())
}

pub trait ListNameSpaceImpl: AsList {
    /// In case the inner dtype [`DataType::Utf8`], the individual items will be joined into a
    /// single string separated by `separator`.
    fn lst_join(&self, separator: &str) -> PolarsResult<Utf8Chunked> {
        let ca = self.as_list();
        match ca.inner_dtype() {
            DataType::Utf8 => {
                // used to amortize heap allocs
                let mut buf = String::with_capacity(128);

                let mut builder = Utf8ChunkedBuilder::new(
                    ca.name(),
                    ca.len(),
                    ca.get_values_size() + separator.len() * ca.len(),
                );

                ca.amortized_iter().for_each(|opt_s| {
                    let opt_val = opt_s.map(|s| {
                        // make sure that we don't write values of previous iteration
                        buf.clear();
                        let ca = s.as_ref().utf8().unwrap();
                        let iter = ca.into_iter().map(|opt_v| opt_v.unwrap_or("null"));

                        for val in iter {
                            buf.write_str(val).unwrap();
                            buf.write_str(separator).unwrap();
                        }
                        // last value should not have a separator, so slice that off
                        // saturating sub because there might have been nothing written.
                        &buf[..buf.len().saturating_sub(separator.len())]
                    });
                    builder.append_option(opt_val)
                });
                Ok(builder.finish())
            }
            dt => polars_bail!(op = "`lst.join`", got = dt, expected = "Utf8"),
        }
    }

    fn lst_max(&self) -> Series {
        list_max_function(self.as_list())
    }

    fn lst_min(&self) -> Series {
        list_min_function(self.as_list())
    }

    fn lst_sum(&self) -> Series {
        fn inner(ca: &ListChunked, inner_dtype: &DataType) -> Series {
            use DataType::*;
            // TODO: add fast path for smaller ints?
            let mut out = match inner_dtype {
                Boolean => {
                    let out: IdxCa = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                UInt32 => {
                    let out: UInt32Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                UInt64 => {
                    let out: UInt64Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                Int32 => {
                    let out: Int32Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                Int64 => {
                    let out: Int64Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                Float32 => {
                    let out: Float32Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                Float64 => {
                    let out: Float64Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().sum()))
                        .collect();
                    out.into_series()
                }
                // slowest sum_as_series path
                _ => ca
                    .apply_amortized(|s| s.as_ref().sum_as_series())
                    .explode()
                    .unwrap()
                    .into_series(),
            };
            out.rename(ca.name());
            out
        }

        let ca = self.as_list();

        if has_inner_nulls(ca) {
            return inner(ca, &ca.inner_dtype());
        };

        match ca.inner_dtype() {
            DataType::Boolean => count_boolean_bits(ca).into_series(),
            dt if dt.is_numeric() => sum_list_numerical(ca, &dt),
            dt => inner(ca, &dt),
        }
    }

    fn lst_mean(&self) -> Series {
        fn inner(ca: &ListChunked) -> Series {
            let mut out: Float64Chunked = ca
                .amortized_iter()
                .map(|s| s.and_then(|s| s.as_ref().mean()))
                .collect();

            out.rename(ca.name());
            out.into_series()
        }
        use DataType::*;

        let ca = self.as_list();

        if has_inner_nulls(ca) {
            return match ca.inner_dtype() {
                Float32 => {
                    let mut out: Float32Chunked = ca
                        .amortized_iter()
                        .map(|s| s.and_then(|s| s.as_ref().mean().map(|v| v as f32)))
                        .collect();

                    out.rename(ca.name());
                    out.into_series()
                }
                _ => inner(ca),
            };
        };

        match ca.inner_dtype() {
            dt if dt.is_numeric() => mean_list_numerical(ca, &dt),
            _ => inner(ca),
        }
    }

    #[must_use]
    fn lst_sort(&self, options: SortOptions) -> ListChunked {
        let ca = self.as_list();
        ca.apply_amortized(|s| s.as_ref().sort_with(options))
    }

    #[must_use]
    fn lst_reverse(&self) -> ListChunked {
        let ca = self.as_list();
        ca.apply_amortized(|s| s.as_ref().reverse())
    }

    fn lst_unique(&self) -> PolarsResult<ListChunked> {
        let ca = self.as_list();
        ca.try_apply_amortized(|s| s.as_ref().unique())
    }

    fn lst_unique_stable(&self) -> PolarsResult<ListChunked> {
        let ca = self.as_list();
        ca.try_apply_amortized(|s| s.as_ref().unique_stable())
    }

    fn lst_arg_min(&self) -> IdxCa {
        let ca = self.as_list();
        let mut out: IdxCa = ca
            .amortized_iter()
            .map(|opt_s| opt_s.and_then(|s| s.as_ref().arg_min().map(|idx| idx as IdxSize)))
            .collect_trusted();
        out.rename(ca.name());
        out
    }

    fn lst_arg_max(&self) -> IdxCa {
        let ca = self.as_list();
        let mut out: IdxCa = ca
            .amortized_iter()
            .map(|opt_s| opt_s.and_then(|s| s.as_ref().arg_max().map(|idx| idx as IdxSize)))
            .collect_trusted();
        out.rename(ca.name());
        out
    }

    #[cfg(feature = "diff")]
    fn lst_diff(&self, n: i64, null_behavior: NullBehavior) -> PolarsResult<ListChunked> {
        let ca = self.as_list();
        ca.try_apply_amortized(|s| s.as_ref().diff(n, null_behavior))
    }

    fn lst_shift(&self, periods: i64) -> ListChunked {
        let ca = self.as_list();
        ca.apply_amortized(|s| s.as_ref().shift(periods))
    }

    fn lst_slice(&self, offset: i64, length: usize) -> ListChunked {
        let ca = self.as_list();
        ca.apply_amortized(|s| s.as_ref().slice(offset, length))
    }

    fn lst_lengths(&self) -> IdxCa {
        let ca = self.as_list();
        let mut lengths = Vec::with_capacity(ca.len());
        ca.downcast_iter().for_each(|arr| {
            let offsets = arr.offsets().as_slice();
            let mut last = offsets[0];
            for o in &offsets[1..] {
                lengths.push((*o - last) as IdxSize);
                last = *o;
            }
        });
        IdxCa::from_vec(ca.name(), lengths)
    }

    /// Get the value by index in the sublists.
    /// So index `0` would return the first item of every sublist
    /// and index `-1` would return the last item of every sublist
    /// if an index is out of bounds, it will return a `None`.
    fn lst_get(&self, idx: i64) -> PolarsResult<Series> {
        let ca = self.as_list();
        let chunks = ca
            .downcast_iter()
            .map(|arr| sublist_get(arr, idx))
            .collect::<Vec<_>>();
        Series::try_from((ca.name(), chunks))
            .unwrap()
            .cast(&ca.inner_dtype())
    }

    #[cfg(feature = "list_take")]
    fn lst_take(&self, idx: &Series, null_on_oob: bool) -> PolarsResult<Series> {
        let list_ca = self.as_list();

        let index_typed_index = |idx: &Series| {
            let idx = idx.cast(&IDX_DTYPE).unwrap();
            list_ca
                .amortized_iter()
                .map(|s| {
                    s.map(|s| {
                        let s = s.as_ref();
                        take_series(s, idx.clone(), null_on_oob)
                    })
                    .transpose()
                })
                .collect::<PolarsResult<ListChunked>>()
                .map(|mut ca| {
                    ca.rename(list_ca.name());
                    ca.into_series()
                })
        };

        use DataType::*;
        match idx.dtype() {
            List(_) => {
                let idx_ca = idx.list().unwrap();
                let mut out = list_ca
                    .amortized_iter()
                    .zip(idx_ca.into_iter())
                    .map(|(opt_s, opt_idx)| {
                        {
                            match (opt_s, opt_idx) {
                                (Some(s), Some(idx)) => {
                                    Some(take_series(s.as_ref(), idx, null_on_oob))
                                }
                                _ => None,
                            }
                        }
                        .transpose()
                    })
                    .collect::<PolarsResult<ListChunked>>()?;
                out.rename(list_ca.name());

                Ok(out.into_series())
            }
            UInt32 | UInt64 => index_typed_index(idx),
            dt if dt.is_signed() => {
                if let Some(min) = idx.min::<i64>() {
                    if min >= 0 {
                        index_typed_index(idx)
                    } else {
                        let mut out = list_ca
                            .amortized_iter()
                            .map(|opt_s| {
                                opt_s
                                    .map(|s| take_series(s.as_ref(), idx.clone(), null_on_oob))
                                    .transpose()
                            })
                            .collect::<PolarsResult<ListChunked>>()?;
                        out.rename(list_ca.name());
                        Ok(out.into_series())
                    }
                } else {
                    polars_bail!(ComputeError: "all indices are null");
                }
            }
            dt => polars_bail!(ComputeError: "cannot use dtype `{}` as an index", dt),
        }
    }

    fn lst_concat(&self, other: &[Series]) -> PolarsResult<ListChunked> {
        let ca = self.as_list();
        let other_len = other.len();
        let length = ca.len();
        let mut other = other.to_vec();
        let mut inner_super_type = ca.inner_dtype();

        for s in &other {
            match s.dtype() {
                DataType::List(inner_type) => {
                    inner_super_type = try_get_supertype(&inner_super_type, inner_type)?;
                    #[cfg(feature = "dtype-categorical")]
                    if let DataType::Categorical(_) = &inner_super_type {
                        inner_super_type = merge_dtypes(&inner_super_type, inner_type)?;
                    }
                }
                dt => {
                    inner_super_type = try_get_supertype(&inner_super_type, dt)?;
                    #[cfg(feature = "dtype-categorical")]
                    if let DataType::Categorical(_) = &inner_super_type {
                        inner_super_type = merge_dtypes(&inner_super_type, dt)?;
                    }
                }
            }
        }

        // cast lhs
        let dtype = &DataType::List(Box::new(inner_super_type.clone()));
        let ca = ca.cast(dtype)?;
        let ca = ca.list().unwrap();

        // broadcasting path in case all unit length
        // this path will not expand the series, so saves memory
        let out = if other.iter().all(|s| s.len() == 1) && ca.len() != 1 {
            cast_rhs(&mut other, &inner_super_type, dtype, length, false)?;
            let to_append = other
                .iter()
                .flat_map(|s| {
                    let lst = s.list().unwrap();
                    lst.get(0)
                })
                .collect::<Vec<_>>();
            // there was a None, so all values will be None
            if to_append.len() != other_len {
                return Ok(ListChunked::full_null_with_dtype(
                    ca.name(),
                    length,
                    &inner_super_type,
                ));
            }

            let vals_size_other = other
                .iter()
                .map(|s| s.list().unwrap().get_values_size())
                .sum::<usize>();

            let mut builder = get_list_builder(
                &inner_super_type,
                ca.get_values_size() + vals_size_other + 1,
                length,
                ca.name(),
            )?;
            ca.into_iter().for_each(|opt_s| {
                let opt_s = opt_s.map(|mut s| {
                    for append in &to_append {
                        s.append(append).unwrap();
                    }
                    match inner_super_type {
                        // structs don't have chunks, so we must first rechunk the underlying series
                        #[cfg(feature = "dtype-struct")]
                        DataType::Struct(_) => s = s.rechunk(),
                        // nothing
                        _ => {}
                    }
                    s
                });
                builder.append_opt_series(opt_s.as_ref())
            });
            builder.finish()
        } else {
            // normal path which may contain same length list or unit length lists
            cast_rhs(&mut other, &inner_super_type, dtype, length, true)?;

            let vals_size_other = other
                .iter()
                .map(|s| s.list().unwrap().get_values_size())
                .sum::<usize>();
            let mut iters = Vec::with_capacity(other_len + 1);

            for s in other.iter_mut() {
                iters.push(s.list()?.amortized_iter())
            }
            let mut first_iter = ca.into_iter();
            let mut builder = get_list_builder(
                &inner_super_type,
                ca.get_values_size() + vals_size_other + 1,
                length,
                ca.name(),
            )?;

            for _ in 0..ca.len() {
                let mut acc = match first_iter.next().unwrap() {
                    Some(s) => s,
                    None => {
                        builder.append_null();
                        // make sure that the iterators advance before we continue
                        for it in &mut iters {
                            it.next().unwrap();
                        }
                        continue;
                    }
                };

                let mut has_nulls = false;
                for it in &mut iters {
                    match it.next().unwrap() {
                        Some(s) => {
                            if !has_nulls {
                                acc.append(s.as_ref())?;
                            }
                        }
                        None => {
                            has_nulls = true;
                        }
                    }
                }
                if has_nulls {
                    builder.append_null();
                    continue;
                }

                match inner_super_type {
                    // structs don't have chunks, so we must first rechunk the underlying series
                    #[cfg(feature = "dtype-struct")]
                    DataType::Struct(_) => acc = acc.rechunk(),
                    // nothing
                    _ => {}
                }
                builder.append_series(&acc);
            }
            builder.finish()
        };
        Ok(out)
    }
}

impl ListNameSpaceImpl for ListChunked {}

#[cfg(feature = "list_take")]
fn take_series(s: &Series, idx: Series, null_on_oob: bool) -> PolarsResult<Series> {
    let len = s.len();
    let idx = cast_index(idx, len, null_on_oob)?;
    let idx = idx.idx().unwrap();
    s.take(idx)
}

#[cfg(feature = "list_take")]
fn cast_signed_index_ca<T: PolarsNumericType>(idx: &ChunkedArray<T>, len: usize) -> Series
where
    T::Native: Copy + PartialOrd + PartialEq + NumCast + Signed + Zero,
{
    idx.into_iter()
        .map(|opt_idx| opt_idx.and_then(|idx| idx.negative_to_usize(len).map(|idx| idx as IdxSize)))
        .collect::<IdxCa>()
        .into_series()
}

#[cfg(feature = "list_take")]
fn cast_unsigned_index_ca<T: PolarsNumericType>(idx: &ChunkedArray<T>, len: usize) -> Series
where
    T::Native: Copy + PartialOrd + ToPrimitive,
{
    idx.into_iter()
        .map(|opt_idx| {
            opt_idx.and_then(|idx| {
                let idx = idx.to_usize().unwrap();
                if idx >= len {
                    None
                } else {
                    Some(idx as IdxSize)
                }
            })
        })
        .collect::<IdxCa>()
        .into_series()
}

#[cfg(feature = "list_take")]
fn cast_index(idx: Series, len: usize, null_on_oob: bool) -> PolarsResult<Series> {
    let idx_null_count = idx.null_count();
    use DataType::*;
    let out = match idx.dtype() {
        #[cfg(feature = "big_idx")]
        UInt32 => {
            if null_on_oob {
                let a = idx.u32().unwrap();
                cast_unsigned_index_ca(a, len)
            } else {
                idx.cast(&IDX_DTYPE).unwrap()
            }
        }
        #[cfg(feature = "big_idx")]
        UInt64 => {
            if null_on_oob {
                let a = idx.u64().unwrap();
                cast_unsigned_index_ca(a, len)
            } else {
                idx
            }
        }
        #[cfg(not(feature = "big_idx"))]
        UInt64 => {
            if null_on_oob {
                let a = idx.u64().unwrap();
                cast_unsigned_index_ca(a, len)
            } else {
                idx.cast(&IDX_DTYPE).unwrap()
            }
        }
        #[cfg(not(feature = "big_idx"))]
        UInt32 => {
            if null_on_oob {
                let a = idx.u32().unwrap();
                cast_unsigned_index_ca(a, len)
            } else {
                idx
            }
        }
        dt if dt.is_unsigned() => idx.cast(&IDX_DTYPE).unwrap(),
        Int8 => {
            let a = idx.i8().unwrap();
            cast_signed_index_ca(a, len)
        }
        Int16 => {
            let a = idx.i16().unwrap();
            cast_signed_index_ca(a, len)
        }
        Int32 => {
            let a = idx.i32().unwrap();
            cast_signed_index_ca(a, len)
        }
        Int64 => {
            let a = idx.i64().unwrap();
            cast_signed_index_ca(a, len)
        }
        _ => {
            unreachable!()
        }
    };
    polars_ensure!(
        out.null_count() == idx_null_count || null_on_oob,
        ComputeError: "take indices are out of bounds"
    );
    Ok(out)
}
