use std::mem::MaybeUninit;
use std::ops::Range;

use rten_tensor::{Matrix, MatrixLayout, Storage};

use super::round_up;

/// Pack a block of the "A" matrix for use by a GEMM kernel.
///
/// The packed buffer is laid out as a sequence of `ceil(rows.len() / MR)`
/// row panels. Each row panel has size `MR * cols.len()` and uses
/// column-major order. If `rows.len()` is not a multiple of `MR`, the
/// final panel is zero-padded.
///
/// # Safety
///
/// When this function returns, all elements of `out` will have been initialized
/// either to a value from `a`, or zero.
#[inline] // Allow caller to control `target_feature`s
pub fn pack_a_block<const MR: usize>(
    out: &mut [MaybeUninit<f32>],
    a: Matrix,
    rows: Range<usize>,
    cols: Range<usize>,
) {
    let a_rows = rows.len();
    let a_cols = cols.len();

    // Safety: Loops below must only access valid offsets in `a_data`.
    let a_data = a.storage();

    let row_stride = a.row_stride();
    let col_stride = a.col_stride();

    let n_panels = round_up(a_rows, MR) / MR;
    for panel in 0..n_panels {
        let panel_offset = panel * a_cols * MR;
        let panel_start_row = panel * MR;

        if a_rows - panel_start_row >= MR {
            // Optimized loop for panels that don't need any padding
            let a_offset = (rows.start + panel_start_row) * row_stride + cols.start * col_stride;

            assert!(out.len() > panel_offset + (a_cols - 1) * MR + MR - 1);
            assert!(a_data.len() > a_offset + (MR - 1) * row_stride + (a_cols - 1) * col_stride);

            // Optimize for common case of unit stride as this generates better
            // code.
            if col_stride == 1 {
                for col in 0..a_cols {
                    for row in 0..MR {
                        // Safety: Indexes are less than lengths asserted above.
                        unsafe {
                            out.get_unchecked_mut(panel_offset + col * MR + row)
                                .write(*a_data.get_unchecked(a_offset + row * row_stride + col));
                        }
                    }
                }
            } else {
                for col in 0..a_cols {
                    for row in 0..MR {
                        // Safety: Indexes are less than lengths asserted above.
                        unsafe {
                            out.get_unchecked_mut(panel_offset + col * MR + row).write(
                                *a_data
                                    .get_unchecked(a_offset + row * row_stride + col * col_stride),
                            );
                        }
                    }
                }
            }
        } else {
            // Fallback for final panel if padding is required
            for col in 0..a_cols {
                let out_col_offset = panel_offset + col * MR;
                for row in 0..MR {
                    let a_row = rows.start + panel_start_row + row;
                    out[out_col_offset + row].write(if a_row < rows.end {
                        let offset = a_row * row_stride + (cols.start + col) * col_stride;
                        unsafe { *a_data.get_unchecked(offset) }
                    } else {
                        0.0
                    });
                }
            }
        }
    }

    // Initialize any spare capacity in the buffer.
    let n_init = n_panels * a_cols * MR;
    for x in &mut out[n_init..] {
        x.write(0.);
    }
}

/// Pack a block of the "B" matrix for use by a GEMM kernel.
///
/// The packed buffer is laid out as a sequence of `ceil(cols.len() /
/// NR)` column panels. Each column panel has size `rows.len() *
/// NR` and uses row-major order. If `cols.len()` is not a multiple of
/// `NR`, the final panel is zero-padded.
///
/// # Safety
///
/// When this function returns, all elements of `out` will have been initialized
/// either to a value from `b`, or zero.
#[inline] // Allow caller to control `target_feature`s
pub fn pack_b_block<const NR: usize>(
    out: &mut [MaybeUninit<f32>],
    b: Matrix,
    rows: Range<usize>,
    cols: Range<usize>,
) {
    let b_cols = cols.len();
    let b_rows = rows.len();
    let b_row_stride = b.row_stride();
    let b_col_stride = b.col_stride();

    // Safety: Loops below must only access valid offsets in `b_data`.
    let b_data = b.storage();

    let n_panels = round_up(b_cols, NR) / NR;
    for panel in 0..n_panels {
        let panel_offset = panel * b_rows * NR;
        let panel_start_col = panel * NR;

        if b_cols - panel_start_col >= NR {
            // Optimized loop for panels that don't need any padding
            let b_offset =
                rows.start * b_row_stride + (cols.start + panel_start_col) * b_col_stride;

            assert!(out.len() >= panel_offset + (b_rows - 1) * NR + NR);
            assert!(
                b_data.len() > b_offset + (b_rows - 1) * b_row_stride + (NR - 1) * b_col_stride
            );

            // Optimize for common case of unit stride, as this makes the inner
            // loop a simple memcpy for which the compiler generates much better
            // code.
            if b_col_stride == 1 {
                for row in 0..b_rows {
                    let out_offset = panel_offset + row * NR;
                    let in_offset = b_offset + row * b_row_stride;
                    for col in 0..NR {
                        // Safety: Indexes are less than lengths asserted above.
                        unsafe {
                            out.get_unchecked_mut(out_offset + col)
                                .write(*b_data.get_unchecked(in_offset + col));
                        }
                    }
                }
            } else {
                for row in 0..b_rows {
                    let out_offset = panel_offset + row * NR;
                    let in_offset = b_offset + row * b_row_stride;
                    for col in 0..NR {
                        // Safety: Indexes are less than lengths asserted above.
                        unsafe {
                            out.get_unchecked_mut(out_offset + col)
                                .write(*b_data.get_unchecked(in_offset + col * b_col_stride));
                        }
                    }
                }
            }
        } else {
            // Fallback for final panel if padding is required
            for row in 0..b_rows {
                let out_row_offset = panel_offset + row * NR;
                let b_row_offset = (rows.start + row) * b_row_stride;

                for col in 0..NR {
                    let out_col = panel_start_col + col;
                    let b_offset =
                        b_row_offset + (cols.start + panel_start_col + col) * b_col_stride;

                    out[out_row_offset + col].write(if out_col < b_cols {
                        unsafe { *b_data.get_unchecked(b_offset) }
                    } else {
                        0.0
                    });
                }
            }
        }
    }

    // Initialize any spare capacity in the buffer.
    let n_init = n_panels * b_rows * NR;
    for x in &mut out[n_init..] {
        x.write(0.);
    }
}
