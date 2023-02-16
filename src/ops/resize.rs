use std::iter::zip;

use crate::check_dims;
use crate::ops::{Input, InputList, IntoOpResult, OpError, Operator, Output};
use crate::tensor::{Tensor, TensorLayout, TensorView, TensorViewMut};

/// Specifies an output size for a resize operation.
pub enum ResizeTarget<'a> {
    /// Vector of scale factors for each dimension. The length should match the
    /// input rank.
    Scales(&'a Tensor),

    /// Vector of output sizes for each dimension. The length should match the
    /// input rank.
    Sizes(&'a Tensor<i32>),
}

struct Image<'a> {
    data: &'a [f32],
    height: usize,
    width: usize,
    h_stride: usize,
    w_stride: usize,
}

impl<'a> From<TensorView<'a, f32>> for Image<'a> {
    /// Convert a 2D tensor view into an [Image].
    fn from(view: TensorView<'a, f32>) -> Image<'a> {
        assert!(view.ndim() == 2, "Tensor must have 2 dims");
        Image {
            data: view.to_data(),
            height: view.shape()[0],
            width: view.shape()[1],
            h_stride: view.stride(0),
            w_stride: view.stride(1),
        }
    }
}

struct ImageMut<'a> {
    data: &'a mut [f32],
    height: usize,
    width: usize,
    h_stride: usize,
    w_stride: usize,
}

impl<'a> From<TensorViewMut<'a, f32>> for ImageMut<'a> {
    /// Convert a 2D tensor view into an [ImageMut]
    fn from(view: TensorViewMut<'a, f32>) -> ImageMut<'a> {
        assert!(view.ndim() == 2, "Tensor must have 2 dims");
        ImageMut {
            height: view.shape()[0],
            width: view.shape()[1],
            h_stride: view.stride(0),
            w_stride: view.stride(1),
            data: view.into_data_mut(),
        }
    }
}

/// Compute the input image coordinate that corresponds to an output coordinate,
/// where `scale` is the scale factor from output to input.
///
/// ONNX supports several modes for transforming coords, specified by the
/// `coordinate_transformation_mode` attribute. The default, implemented here,
/// is the "half pixel" mode. The half pixel mode is consistent with how OpenCV
/// (`cv2.resize`) and PyTorch (`torch.nn.functional.interpolate`) work. See
/// https://jricheimer.github.io/tensorflow/2019/02/11/resize-confusion/
/// for rationale.
fn input_coord(dest_coord: usize, scale: f32, mode: CoordTransformMode) -> f32 {
    type Ctm = CoordTransformMode;
    match mode {
        Ctm::HalfPixel => scale * (dest_coord as f32 + 0.5) - 0.5,
        Ctm::Asymmetric => scale * dest_coord as f32,
    }
}

/// Specifies how resizing with `ResizeMode::Nearest` should map a fractional
/// input coordinate to an image coordinate.
#[derive(Copy, Clone, Debug, Default)]
pub enum NearestMode {
    Ceil,
    Floor,
    RoundPreferCeil,

    #[default]
    RoundPreferFloor,
}

/// Specifies how resizing maps output coordinates to input coordinates.
#[derive(Copy, Clone, Debug, Default)]
pub enum CoordTransformMode {
    #[default]
    HalfPixel,
    Asymmetric,
}

fn nearest_resize(
    input: &Image,
    output: &mut ImageMut,
    mode: NearestMode,
    coord_mode: CoordTransformMode,
) {
    // Scale factors to map output coords to input coords.
    let inv_scale_y = input.height as f32 / output.height as f32;
    let inv_scale_x = input.width as f32 / output.width as f32;

    let round_coord = |coord: f32| match mode {
        NearestMode::Ceil => coord.ceil() as usize,
        NearestMode::Floor => coord as usize,

        // `f32::round` has round-away-from-zero behavior. For `RoundPreferCeil`
        // and `RoundPreferFloor` we need to always round up or down.
        NearestMode::RoundPreferCeil => {
            if coord.fract() == 0.5 {
                coord.ceil() as usize
            } else {
                coord.round() as usize
            }
        }
        NearestMode::RoundPreferFloor => {
            if coord.fract() == 0.5 {
                coord.floor() as usize
            } else {
                coord.round() as usize
            }
        }
    };

    for y in 0..output.height {
        let in_y = round_coord(
            input_coord(y, inv_scale_y, coord_mode).clamp(0., input.height as f32 - 1.),
        );
        for x in 0..output.width {
            let in_x = round_coord(
                input_coord(x, inv_scale_x, coord_mode).clamp(0., input.width as f32 - 1.),
            );
            let out = input.data[in_y * input.h_stride + in_x * input.w_stride];
            output.data[y * output.h_stride + x * output.w_stride] = out;
        }
    }
}

fn bilinear_resize(input: &Image, output: &mut ImageMut, coord_mode: CoordTransformMode) {
    // Scale factors to map output coords to input coords.
    let inv_scale_y = input.height as f32 / output.height as f32;
    let inv_scale_x = input.width as f32 / output.width as f32;

    for y in 0..output.height {
        let in_y = input_coord(y, inv_scale_y, coord_mode).clamp(0., input.height as f32 - 1.);
        let in_y1 = in_y as usize;
        let in_y2 = (in_y1 + 1).min(input.height - 1);
        let weight_y = in_y - (in_y1 as f32);

        for x in 0..output.width {
            let in_x = input_coord(x, inv_scale_x, coord_mode).clamp(0., input.width as f32 - 1.);
            let in_x1 = in_x as usize;
            let in_x2 = (in_x1 + 1).min(input.width - 1);
            let weight_x = in_x - (in_x1 as f32);

            let in_tl = input.data[in_y1 * input.h_stride + in_x1 * input.w_stride];
            let in_tr = input.data[in_y1 * input.h_stride + in_x2 * input.w_stride];
            let in_bl = input.data[in_y2 * input.h_stride + in_x1 * input.w_stride];
            let in_br = input.data[in_y2 * input.h_stride + in_x2 * input.w_stride];

            // Interpolate in X direction
            let out_top = (1. - weight_x) * in_tl + weight_x * in_tr;
            let out_bottom = (1. - weight_x) * in_bl + weight_x * in_br;

            // Interpolate in Y direction
            let out = (1. - weight_y) * out_top + weight_y * out_bottom;

            output.data[y * output.h_stride + x * output.w_stride] = out;
        }
    }
}

pub fn resize(
    input: &Tensor,
    target: ResizeTarget,
    mode: ResizeMode,
    coord_mode: CoordTransformMode,
    nearest_mode: NearestMode,
) -> Result<Tensor, OpError> {
    let sizes = match target {
        ResizeTarget::Scales(scales) => {
            check_dims!(scales, 1);
            zip(input.shape().iter(), scales.iter())
                .map(|(&in_size, scale)| ((in_size as f32) * scale).floor() as i32)
                .collect()
        }
        ResizeTarget::Sizes(sizes) => {
            check_dims!(sizes, 1);
            sizes.clone()
        }
    };

    if sizes.len() != input.ndim() {
        return Err(OpError::IncompatibleInputShapes(
            "scales/sizes length should equal input rank",
        ));
    }
    if sizes.iter().any(|size| size < 0) {
        return Err(OpError::InvalidValue("scales/sizes must be positive"));
    }

    // The current implementation only supports NCHW tensors with scale factors
    // other than 1.0 for the H and W dims.
    let [batch, chans, _height, _width] = check_dims!(input, 4, "NCHW");
    let sizes_valid = zip(0..input.ndim(), input.shape().iter()).all(|(dim, &in_size)| {
        dim == input.ndim() - 1 || dim == input.ndim() - 2 || sizes[[dim]] == in_size as i32
    });
    if !sizes_valid {
        return Err(OpError::UnsupportedValue(
            "only height and width dimensions can be resized",
        ));
    }

    let sizes_usize: Vec<_> = sizes.iter().map(|size| size as usize).collect();
    let mut output = Tensor::zeros(&sizes_usize);

    if output.is_empty() {
        return Ok(output);
    }

    for n in 0..batch {
        for c in 0..chans {
            let in_image: Image = input.slice(&[n.into(), c.into()]).into();
            let mut out_view = output.view_mut();
            let mut out_image: ImageMut = out_view.slice_mut(&[n.into(), c.into()]).into();
            match mode {
                ResizeMode::Nearest => {
                    nearest_resize(&in_image, &mut out_image, nearest_mode, coord_mode);
                }
                ResizeMode::Linear => {
                    bilinear_resize(&in_image, &mut out_image, coord_mode);
                }
            };
        }
    }

    Ok(output)
}

#[derive(Clone, Copy, Debug, Default)]
pub enum ResizeMode {
    #[default]
    Nearest,
    Linear,
}

#[derive(Debug)]
pub struct Resize {
    pub mode: ResizeMode,
    pub coord_mode: CoordTransformMode,
    pub nearest_mode: NearestMode,
}

impl Default for Resize {
    fn default() -> Resize {
        Resize {
            mode: ResizeMode::Nearest,
            coord_mode: CoordTransformMode::default(),
            nearest_mode: NearestMode::default(),
        }
    }
}

/// Get an optional input for the Resize operator, treating empty tensors as
/// missing inputs.
///
/// This is needed for compatibility with ONNX models generated by PyTorch when
/// targeting opset < 13. See https://github.com/pytorch/pytorch/pull/50574.
fn get_optional_input<'a, T: Copy>(
    inputs: &InputList<'a>,
    index: usize,
) -> Result<Option<&'a Tensor<T>>, OpError>
where
    &'a Tensor<T>: TryFrom<Input<'a>, Error = OpError>,
{
    let tensor = inputs.get_as(index)?.filter(|t| !t.is_empty());
    Ok(tensor)
}

impl Operator for Resize {
    fn name(&self) -> &str {
        "Resize"
    }

    fn run(&self, inputs: InputList) -> Result<Vec<Output>, OpError> {
        let input = inputs.require_as(0)?;

        // The `roi` input is only used if the `coordinate_transformation_mode`
        // ONNX attr is `tf_crop_and_resize`, which is not currently supported.
        let _roi = get_optional_input::<f32>(&inputs, 1)?;

        let scales = get_optional_input(&inputs, 2)?.map(ResizeTarget::Scales);
        let sizes = get_optional_input(&inputs, 3)?.map(ResizeTarget::Sizes);
        let target = scales.or(sizes).ok_or(OpError::MissingInputs)?;

        resize(input, target, self.mode, self.coord_mode, self.nearest_mode).into_op_result()
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::{
        resize, CoordTransformMode, InputList, NearestMode, OpError, Operator, Resize, ResizeMode,
        ResizeTarget,
    };
    use crate::tensor::{Tensor, TensorLayout};
    use crate::test_util::expect_equal;

    // Reference values for these tests can be computed with either OpenCV
    // (`cv2.resize`) or PyTorch (`torch.nn.functional.interpolate`).

    #[test]
    fn test_resize_nearest() -> Result<(), String> {
        struct Case {
            image: Tensor,
            scales: Vec<f32>,
            expected: Tensor,
        }

        let cases = [
            // Scale width and height by 0x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 0., 0.],
                expected: Tensor::from_data(&[1, 1, 0, 0], vec![]),
            },
            // Scale width and height by 0.5x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 0.5, 0.5],
                expected: Tensor::from_data(&[1, 1, 1, 1], vec![0.2]),
            },
            // Scale width and height by 1x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 1., 1.],
                expected: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
            },
            // Scale width and height by 1.5x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 1.5, 1.5],
                expected: Tensor::from_data(
                    &[1, 1, 3, 3],
                    vec![
                        0.2000, 0.2000, 0.7000, // Y=0
                        0.2000, 0.2000, 0.7000, // Y=1
                        0.3000, 0.3000, 0.8000, // Y=2
                    ],
                ),
            },
            // Scale width and height by 2x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 2., 2.],
                expected: Tensor::from_data(
                    &[1, 1, 4, 4],
                    vec![
                        0.2, 0.2, 0.7, 0.7, // Y=0
                        0.2, 0.2, 0.7, 0.7, // Y=1
                        0.3, 0.3, 0.8, 0.8, // Y=2
                        0.3, 0.3, 0.8, 0.8, // Y=3
                    ],
                ),
            },
            // Scale width and height by 3x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 3., 3.],
                expected: Tensor::from_data(
                    &[1, 1, 6, 6],
                    vec![
                        0.2000, 0.2000, 0.2000, 0.7000, 0.7000, 0.7000, // Y=0
                        0.2000, 0.2000, 0.2000, 0.7000, 0.7000, 0.7000, // Y=1
                        0.2000, 0.2000, 0.2000, 0.7000, 0.7000, 0.7000, // Y=2
                        0.3000, 0.3000, 0.3000, 0.8000, 0.8000, 0.8000, // Y=3
                        0.3000, 0.3000, 0.3000, 0.8000, 0.8000, 0.8000, // Y=4
                        0.3000, 0.3000, 0.3000, 0.8000, 0.8000, 0.8000, // Y=5
                    ],
                ),
            },
        ];

        for case in cases {
            let scales = Tensor::from_vec(case.scales);
            let result = resize(
                &case.image,
                ResizeTarget::Scales(&scales),
                ResizeMode::Nearest,
                CoordTransformMode::HalfPixel,
                NearestMode::RoundPreferFloor,
            )
            .unwrap();

            expect_equal(&result, &case.expected)?;
        }

        Ok(())
    }

    #[test]
    fn test_resize_nearest_mode() -> Result<(), String> {
        let image = Tensor::from_data(&[1, 1, 1, 2], vec![0.1, 0.2]);

        // Use a scale factor of 4 so that we have output pixels that map
        // to input coordinates with fractional values of 0, 0.25, 0.5 and 0.75.
        // This allows the same input to exercise all the rounding modes.
        let scales = Tensor::from_vec(vec![1., 1., 1., 4.]);

        struct Case {
            mode: NearestMode,

            // Expected output after nearest resizing using `mode` and the
            // "asymmetric" output => input coord transform. This coord transform
            // is used because it is the simplest (input_coord = output_coord / scale).
            expected: Tensor,
        }

        let cases = [
            Case {
                mode: NearestMode::Ceil,
                expected: Tensor::from_data(
                    &[1, 1, 1, 8],
                    vec![0.1, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2],
                ),
            },
            Case {
                mode: NearestMode::Floor,
                expected: Tensor::from_data(
                    &[1, 1, 1, 8],
                    vec![0.1, 0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2],
                ),
            },
            Case {
                mode: NearestMode::RoundPreferCeil,
                expected: Tensor::from_data(
                    &[1, 1, 1, 8],
                    vec![0.1, 0.1, 0.2, 0.2, 0.2, 0.2, 0.2, 0.2],
                ),
            },
            Case {
                mode: NearestMode::RoundPreferFloor,
                expected: Tensor::from_data(
                    &[1, 1, 1, 8],
                    vec![0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2, 0.2],
                ),
            },
        ];

        for case in cases {
            let result = resize(
                &image,
                ResizeTarget::Scales(&scales),
                ResizeMode::Nearest,
                CoordTransformMode::Asymmetric,
                case.mode,
            )
            .unwrap();

            expect_equal(&result, &case.expected)?;
        }

        Ok(())
    }

    #[test]
    fn test_resize_bilinear() -> Result<(), String> {
        struct Case {
            image: Tensor,
            scales: Vec<f32>,
            expected: Tensor,
        }

        let cases = [
            // Scale width and height by 0x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 0., 0.],
                expected: Tensor::from_data(&[1, 1, 0, 0], vec![]),
            },
            // Scale width and height by 0.5x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 0.5, 0.5],

                // OpenCV and PyTorch produce different results for this case.
                // This result matches OpenCV. This relates to the `half_pixel`
                // vs `pytorch_half_pixel` values for the `coordinate_transformation_mode`
                // attribute in the ONNX op.
                expected: Tensor::from_data(&[1, 1, 1, 1], vec![0.5]),
            },
            // Scale width and height by 1x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 1., 1.],
                expected: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
            },
            // Scale width and height by 1.5x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 1.5, 1.5],
                expected: Tensor::from_data(
                    &[1, 1, 3, 3],
                    vec![
                        0.2, 0.45, 0.7, // Y=0
                        0.25, 0.5, 0.75, // Y=1
                        0.3, 0.55, 0.8, // Y=2
                    ],
                ),
            },
            // Scale width and height by 2x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 2., 2.],
                expected: Tensor::from_data(
                    &[1, 1, 4, 4],
                    vec![
                        0.2, 0.325, 0.575, 0.7, // Y=0
                        0.225, 0.35, 0.6, 0.725, // Y=1
                        0.275, 0.4, 0.65, 0.775, // Y=2
                        0.3, 0.425, 0.675, 0.8, // Y=3
                    ],
                ),
            },
            // Scale width and height by 3x
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: vec![1., 1., 3., 3.],
                expected: Tensor::from_data(
                    &[1, 1, 6, 6],
                    vec![
                        0.2000, 0.2000, 0.3667, 0.5333, 0.7000, 0.7000, // Y=0
                        0.2000, 0.2000, 0.3667, 0.5333, 0.7000, 0.7000, // Y=1
                        0.2333, 0.2333, 0.4000, 0.5667, 0.7333, 0.7333, // Y=2
                        0.2667, 0.2667, 0.4333, 0.6000, 0.7667, 0.7667, // Y=3
                        0.3000, 0.3000, 0.4667, 0.6333, 0.8000, 0.8000, // Y=4
                        0.3000, 0.3000, 0.4667, 0.6333, 0.8000, 0.8000, // Y=5
                    ],
                ),
            },
        ];

        for case in cases {
            let scales = Tensor::from_vec(case.scales);

            let result = resize(
                &case.image,
                ResizeTarget::Scales(&scales),
                ResizeMode::Linear,
                CoordTransformMode::HalfPixel,
                NearestMode::Floor,
            )
            .unwrap();

            expect_equal(&result, &case.expected)?;
        }

        Ok(())
    }

    #[test]
    fn test_resize_scales_sizes() {
        enum CaseOutput {
            Shape(Vec<usize>),
            Error(OpError),
        }

        struct Case {
            image: Tensor,
            scales: Option<Tensor>,
            sizes: Option<Tensor<i32>>,
            expected: CaseOutput,
        }

        let cases = [
            // Specify output size via `scales`
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: Some(Tensor::from_vec(vec![1., 1., 1., 1.])),
                sizes: None,
                expected: CaseOutput::Shape(vec![1, 1, 1, 1]),
            },
            // Specify output size via `sizes`
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: None,
                sizes: Some(Tensor::from_vec(vec![1, 1, 2, 2])),
                expected: CaseOutput::Shape(vec![1, 1, 2, 2]),
            },
            // At least one of `scales` or `sizes` must be provided
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: None,
                sizes: None,
                expected: CaseOutput::Error(OpError::MissingInputs),
            },
            // Test empty tensors are also treated as missing inputs, for
            // compatibility with PyTorch targeting ONNX opset < 13.
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: Some(Tensor::from_vec(vec![])),
                sizes: Some(Tensor::from_vec(vec![])),
                expected: CaseOutput::Error(OpError::MissingInputs),
            },
            // Invalid values for scales/sizes
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: Some(Tensor::from_vec(vec![1., 1., 1.])),
                sizes: None,
                expected: CaseOutput::Error(OpError::IncompatibleInputShapes(
                    "scales/sizes length should equal input rank",
                )),
            },
            Case {
                image: Tensor::from_data(&[1, 1, 1, 1], vec![1.]),
                scales: Some(Tensor::from_vec(vec![1., 1., -1., 1.])),
                sizes: None,
                expected: CaseOutput::Error(OpError::InvalidValue("scales/sizes must be positive")),
            },
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: Some(Tensor::from_data(&[1, 1, 2, 2], vec![1., 1., 3., 3.])),
                sizes: None,
                expected: CaseOutput::Error(OpError::InvalidValue("scales must have 1 dims")),
            },
            // Values for scales/sizes and input shapes which are legal according to the spec,
            // but not currently supported in our implementation.
            Case {
                image: Tensor::from_data(&[1, 1, 2, 2], vec![0.2, 0.7, 0.3, 0.8]),
                scales: Some(Tensor::from_vec(vec![2., 1., 3., 3.])),
                sizes: None,
                expected: CaseOutput::Error(OpError::UnsupportedValue(
                    "only height and width dimensions can be resized",
                )),
            },
            Case {
                image: Tensor::from_vec(vec![1., 1.]),
                scales: Some(Tensor::from_vec(vec![1.])),
                sizes: None,
                expected: CaseOutput::Error(OpError::InvalidValue("input must have 4 dims (NCHW)")),
            },
        ];

        for case in cases {
            let op = Resize {
                mode: ResizeMode::Linear,
                ..Resize::default()
            };
            let inputs = [
                Some((&case.image).into()),
                None, // `roi`
                case.scales.as_ref().map(|t| t.into()),
                case.sizes.as_ref().map(|t| t.into()),
            ];
            let result = op.run(InputList::from_optional(&inputs));
            match (case.expected, result) {
                (CaseOutput::Shape(shape), Ok(out)) => {
                    let tensor = out[0].as_float_ref().unwrap();
                    assert_eq!(tensor.shape(), &shape);
                }
                (CaseOutput::Error(expected_err), Err(err)) => {
                    assert_eq!(err, expected_err);
                }
                (CaseOutput::Shape(_), Err(err)) => {
                    panic!("Expected output but got error {:?}", err);
                }
                (CaseOutput::Error(_), Ok(_)) => {
                    panic!("Expected error but got output");
                }
            }
        }
    }
}
