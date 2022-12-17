extern crate flatbuffers;

use flatbuffers::{FlatBufferBuilder, UnionWIPOffset, Vector, WIPOffset};

use crate::ops::{
    AveragePool2d, BatchNormalization, Cast, Clip, Concat, ConstantOfShape, Conv2d,
    ConvTranspose2d, DataType, Gather, Gemm, LeakyRelu, MaxPool2d, Padding, ReduceMean, Scalar,
    Softmax, Split, Squeeze, Transpose, Unsqueeze,
};
use crate::schema_generated as sg;
use crate::tensor::Tensor;

/// Enum of all the built-in operators
pub enum OpType {
    Add,
    AveragePool2d(AveragePool2d),
    BatchNormalization(BatchNormalization),
    Cast(Cast),
    Clip(Clip),
    Concat(Concat),
    ConstantOfShape(ConstantOfShape),
    Conv2d(Conv2d),
    ConvTranspose2d(ConvTranspose2d),
    Div,
    Equal,
    Erf,
    Expand,
    Gather(Gather),
    Gemm(Gemm),
    GlobalAveragePool,
    Identity,
    LeakyRelu(LeakyRelu),
    Less,
    MatMul,
    MaxPool2d(MaxPool2d),
    Mul,
    Pad,
    Pow,
    Range,
    ReduceMean(ReduceMean),
    Relu,
    Reshape,
    Shape,
    Sigmoid,
    Slice,
    Softmax(Softmax),
    Split(Split),
    Sqrt,
    Squeeze(Squeeze),
    Sub,
    Transpose(Transpose),
    Unsqueeze(Unsqueeze),
    Where,
}

/// Builds a serialized FlatBuffers representation of a model using the schema
/// defined in schema.fbs.
///
/// This exists for use in model-loading tests. Models for deployment are
/// normally built by converting ONNX models using the Python scripts.
pub struct ModelBuilder<'a> {
    builder: FlatBufferBuilder<'a>,
    nodes: Vec<WIPOffset<sg::Node<'a>>>,
}

enum NodeData<'a> {
    Constant(WIPOffset<sg::ConstantNode<'a>>),
    Value(WIPOffset<sg::ValueNode<'a>>),
    Operator(WIPOffset<sg::OperatorNode<'a>>),
}

struct PadArgs {
    pad_mode: sg::PadMode,
    pads: Option<Vec<usize>>,
}

fn pad_args_from_padding(padding: Padding) -> PadArgs {
    match padding {
        Padding::Same => PadArgs {
            pad_mode: sg::PadMode::Same,
            pads: None,
        },
        Padding::Fixed(pads) => PadArgs {
            pad_mode: sg::PadMode::Fixed,
            pads: Some(pads.into()),
        },
    }
}

impl<'a> ModelBuilder<'a> {
    pub fn new() -> ModelBuilder<'a> {
        let builder = FlatBufferBuilder::with_capacity(1024);
        ModelBuilder {
            builder,
            nodes: Vec::new(),
        }
    }

    fn add_node(&mut self, name: Option<&str>, data: NodeData) -> u32 {
        let (data_type, union_val) = match data {
            NodeData::Constant(offset) => (sg::NodeKind::ConstantNode, offset.as_union_value()),
            NodeData::Value(offset) => (sg::NodeKind::ValueNode, offset.as_union_value()),
            NodeData::Operator(offset) => (sg::NodeKind::OperatorNode, offset.as_union_value()),
        };
        let args = sg::NodeArgs {
            name: name.map(|x| self.builder.create_string(x)),
            data_type,
            data: Some(union_val),
        };
        let node = sg::Node::create(&mut self.builder, &args);
        self.nodes.push(node);
        (self.nodes.len() - 1) as u32
    }

    /// Add a constant node (eg. weights, biases) to the model
    pub fn add_float_constant(&mut self, input: &Tensor) -> u32 {
        let elts: Vec<f32> = input.elements().collect();
        let data_vec = self.builder.create_vector(&elts);

        let float_data = sg::FloatData::create(
            &mut self.builder,
            &sg::FloatDataArgs {
                data: Some(data_vec),
            },
        );

        self.add_constant_node(
            input.shape(),
            sg::ConstantData::FloatData,
            float_data.as_union_value(),
        )
    }

    /// Add a constant node (eg. weights, biases) to the model
    pub fn add_int_constant(&mut self, input: &Tensor<i32>) -> u32 {
        let elts: Vec<i32> = input.elements().collect();
        let data_vec = self.builder.create_vector(&elts);

        let int_data = sg::IntData::create(
            &mut self.builder,
            &sg::IntDataArgs {
                data: Some(data_vec),
            },
        );

        self.add_constant_node(
            input.shape(),
            sg::ConstantData::IntData,
            int_data.as_union_value(),
        )
    }

    fn add_constant_node(
        &mut self,
        shape: &[usize],
        data_type: sg::ConstantData,
        data: WIPOffset<UnionWIPOffset>,
    ) -> u32 {
        let shape: Vec<u32> = shape.iter().map(|&x| x as u32).collect();
        let shape_vec = self.builder.create_vector(&shape[..]);

        let const_node = sg::ConstantNode::create(
            &mut self.builder,
            &sg::ConstantNodeArgs {
                shape: Some(shape_vec),
                data_type,
                data: Some(data),
            },
        );
        self.add_node(None, NodeData::Constant(const_node))
    }

    /// Add a value node to the model
    pub fn add_value(&mut self, id: &str) -> u32 {
        let value_node = sg::ValueNode::create(&mut self.builder, &sg::ValueNodeArgs {});
        self.add_node(Some(id), NodeData::Value(value_node))
    }

    fn create_u32_vec<'fbb>(
        &mut self,
        data: Option<Vec<usize>>,
    ) -> Option<WIPOffset<Vector<'a, u32>>> {
        let vec_u32: Option<Vec<u32>> =
            data.map(|vec| vec.iter().map(|&item| item as u32).collect());
        vec_u32.map(|v| self.builder.create_vector(&v))
    }

    fn create_i32_vec<'fbb>(
        &mut self,
        data: Option<Vec<i32>>,
    ) -> Option<WIPOffset<Vector<'a, i32>>> {
        let vec_i32: Option<Vec<i32>> =
            data.map(|vec| vec.iter().map(|&item| item as i32).collect());
        vec_i32.map(|v| self.builder.create_vector(&v))
    }

    /// Add an operator node to the model
    pub fn add_operator(
        &mut self,
        id: &str,
        op_info: OpType,
        inputs: &[u32],
        outputs: &[u32],
    ) -> u32 {
        type OT = sg::OperatorType;
        type OA = sg::OperatorAttrs;

        // Generate an (op_type, attr_type, attrs) tuple for an operator with
        // no attributes.
        macro_rules! no_attr_op {
            ($op_name:ident) => {
                (OT::$op_name, sg::OperatorAttrs::NONE, None)
            };
        }

        // Translate internal operator info to the types in the schema.
        // There is unfortunately a lot of boilerplate here.
        let (op_type, attrs_type, attrs) = match op_info {
            OpType::Add => no_attr_op!(Add),
            OpType::AveragePool2d(args) => (
                OT::AveragePool2d,
                OA::AveragePool2dAttrs,
                Some({
                    let pad_args = pad_args_from_padding(args.padding);
                    let pads = self.create_u32_vec(pad_args.pads);
                    sg::AveragePool2dAttrs::create(&mut self.builder, {
                        &sg::AveragePool2dAttrsArgs {
                            kernel_size: args.kernel_size as u32,
                            pad_mode: pad_args.pad_mode,
                            pads,
                            stride: args.stride as u32,
                        }
                    })
                    .as_union_value()
                }),
            ),
            OpType::BatchNormalization(args) => (
                OT::BatchNormalization,
                OA::BatchNormalizationAttrs,
                Some(
                    sg::BatchNormalizationAttrs::create(
                        &mut self.builder,
                        &sg::BatchNormalizationAttrsArgs {
                            epsilon: args.epsilon,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Cast(args) => (
                OT::Cast,
                OA::CastAttrs,
                Some(
                    sg::CastAttrs::create(
                        &mut self.builder,
                        &sg::CastAttrsArgs {
                            to: match args.to {
                                DataType::Int32 => sg::DataType::Int32,
                                DataType::Float => sg::DataType::Float,
                            },
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Clip(args) => (
                OT::Clip,
                OA::ClipAttrs,
                Some(
                    sg::ClipAttrs::create(
                        &mut self.builder,
                        &sg::ClipAttrsArgs {
                            min: args.min,
                            max: args.max,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Concat(args) => (
                OT::Concat,
                OA::ConcatAttrs,
                Some(
                    sg::ConcatAttrs::create(
                        &mut self.builder,
                        &sg::ConcatAttrsArgs {
                            dim: args.dim as u32,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::ConstantOfShape(args) => (
                OT::ConstantOfShape,
                OA::ConstantOfShapeAttrs,
                Some({
                    let args = match args.value {
                        Scalar::Int(int_value) => sg::ConstantOfShapeAttrsArgs {
                            value_type: sg::Scalar::IntScalar,
                            value: Some(
                                sg::IntScalar::create(
                                    &mut self.builder,
                                    &sg::IntScalarArgs { value: int_value },
                                )
                                .as_union_value(),
                            ),
                        },
                        Scalar::Float(float_value) => sg::ConstantOfShapeAttrsArgs {
                            value_type: sg::Scalar::FloatScalar,
                            value: Some(
                                sg::FloatScalar::create(
                                    &mut self.builder,
                                    &sg::FloatScalarArgs { value: float_value },
                                )
                                .as_union_value(),
                            ),
                        },
                    };
                    sg::ConstantOfShapeAttrs::create(&mut self.builder, &args).as_union_value()
                }),
            ),
            OpType::Conv2d(args) => (
                OT::Conv2d,
                OA::Conv2dAttrs,
                Some({
                    let pad_args = pad_args_from_padding(args.padding);
                    let pads = self.create_u32_vec(pad_args.pads);
                    sg::Conv2dAttrs::create(&mut self.builder, {
                        &sg::Conv2dAttrsArgs {
                            groups: args.groups as u32,
                            pad_mode: pad_args.pad_mode,
                            pads,
                            stride: args.stride as u32,
                        }
                    })
                    .as_union_value()
                }),
            ),
            OpType::ConvTranspose2d(args) => (
                OT::ConvTranspose2d,
                OA::ConvTranspose2dAttrs,
                Some(
                    sg::ConvTranspose2dAttrs::create(
                        &mut self.builder,
                        &sg::ConvTranspose2dAttrsArgs {
                            stride: args.stride as u32,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Div => no_attr_op!(Div),
            OpType::Equal => no_attr_op!(Equal),
            OpType::Erf => no_attr_op!(Erf),
            OpType::Expand => no_attr_op!(Expand),
            OpType::Gather(args) => (
                OT::Gather,
                OA::GatherAttrs,
                Some(
                    sg::GatherAttrs::create(
                        &mut self.builder,
                        &sg::GatherAttrsArgs {
                            axis: args.axis as u32,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Gemm(args) => (
                OT::Gemm,
                OA::GemmAttrs,
                Some(
                    sg::GemmAttrs::create(
                        &mut self.builder,
                        &sg::GemmAttrsArgs {
                            alpha: args.alpha,
                            beta: args.beta,
                            transpose_a: args.transpose_a,
                            transpose_b: args.transpose_b,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::GlobalAveragePool => no_attr_op!(GlobalAveragePool),
            OpType::Identity => no_attr_op!(Identity),
            OpType::LeakyRelu(args) => (
                OT::LeakyRelu,
                OA::LeakyReluAttrs,
                Some(
                    sg::LeakyReluAttrs::create(
                        &mut self.builder,
                        &sg::LeakyReluAttrsArgs { alpha: args.alpha },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Less => no_attr_op!(Less),
            OpType::MatMul => no_attr_op!(MatMul),
            OpType::MaxPool2d(args) => (
                OT::MaxPool2d,
                OA::MaxPool2dAttrs,
                Some({
                    let pad_args = pad_args_from_padding(args.padding);
                    let pads = self.create_u32_vec(pad_args.pads);
                    sg::MaxPool2dAttrs::create(&mut self.builder, {
                        &sg::MaxPool2dAttrsArgs {
                            kernel_size: args.kernel_size as u32,
                            pad_mode: pad_args.pad_mode,
                            pads,
                            stride: args.stride as u32,
                        }
                    })
                    .as_union_value()
                }),
            ),
            OpType::Mul => no_attr_op!(Mul),
            OpType::Pad => no_attr_op!(Pad),
            OpType::Pow => no_attr_op!(Pow),
            OpType::Range => no_attr_op!(Range),
            OpType::ReduceMean(args) => {
                let axes = self.create_i32_vec(args.axes);
                (
                    OT::ReduceMean,
                    OA::ReduceMeanAttrs,
                    Some(
                        sg::ReduceMeanAttrs::create(
                            &mut self.builder,
                            &sg::ReduceMeanAttrsArgs {
                                axes,
                                keep_dims: args.keep_dims,
                            },
                        )
                        .as_union_value(),
                    ),
                )
            }
            OpType::Relu => no_attr_op!(Relu),
            OpType::Reshape => no_attr_op!(Reshape),
            OpType::Shape => no_attr_op!(Shape),
            OpType::Sigmoid => no_attr_op!(Sigmoid),
            OpType::Slice => no_attr_op!(Slice),
            OpType::Softmax(args) => (
                OT::Softmax,
                OA::SoftmaxAttrs,
                Some(
                    sg::SoftmaxAttrs::create(
                        &mut self.builder,
                        &sg::SoftmaxAttrsArgs {
                            axis: args.axis as u32,
                        },
                    )
                    .as_union_value(),
                ),
            ),
            OpType::Split(args) => {
                let split = self.create_u32_vec(Some(args.split));
                (
                    OT::Split,
                    OA::SplitAttrs,
                    Some(
                        sg::SplitAttrs::create(
                            &mut self.builder,
                            &sg::SplitAttrsArgs {
                                axis: args.axis as i32,
                                split,
                            },
                        )
                        .as_union_value(),
                    ),
                )
            }
            OpType::Sqrt => no_attr_op!(Sqrt),
            OpType::Squeeze(args) => {
                let axes = self.create_u32_vec(args.axes);
                (
                    OT::Squeeze,
                    OA::SqueezeAttrs,
                    Some(
                        sg::SqueezeAttrs::create(&mut self.builder, &sg::SqueezeAttrsArgs { axes })
                            .as_union_value(),
                    ),
                )
            }
            OpType::Sub => no_attr_op!(Sub),
            OpType::Transpose(args) => {
                let perm = self.create_u32_vec(args.perm);
                (
                    OT::Transpose,
                    OA::TransposeAttrs,
                    Some(
                        sg::TransposeAttrs::create(
                            &mut self.builder,
                            &sg::TransposeAttrsArgs { perm },
                        )
                        .as_union_value(),
                    ),
                )
            }
            OpType::Unsqueeze(args) => {
                let axes_u32: Vec<u32> = args.axes.iter().map(|&axis| axis as u32).collect();
                let axes = self.builder.create_vector(&axes_u32);
                (
                    OT::Unsqueeze,
                    OA::UnsqueezeAttrs,
                    Some(
                        sg::UnsqueezeAttrs::create(
                            &mut self.builder,
                            &sg::UnsqueezeAttrsArgs { axes: Some(axes) },
                        )
                        .as_union_value(),
                    ),
                )
            }
            OpType::Where => no_attr_op!(Where),
        };

        let input_vec = self.builder.create_vector(inputs);
        let output_vec = self.builder.create_vector(outputs);
        let op_node = sg::OperatorNode::create(
            &mut self.builder,
            &sg::OperatorNodeArgs {
                type_: op_type,
                attrs_type,
                attrs,
                inputs: Some(input_vec),
                outputs: Some(output_vec),
            },
        );
        self.add_node(Some(id), NodeData::Operator(op_node))
    }

    /// Finish writing the model data to the buffer and return the buffer's contents.
    pub fn finish(mut self) -> Vec<u8> {
        let nodes_vec = self.builder.create_vector(&self.nodes[..]);

        let graph = sg::Graph::create(
            &mut self.builder,
            &sg::GraphArgs {
                nodes: Some(nodes_vec),
            },
        );

        let model = sg::Model::create(
            &mut self.builder,
            &sg::ModelArgs {
                schema_version: 1,
                graph: Some(graph),
            },
        );

        self.builder.finish(model, None);
        self.builder.finished_data().to_vec()
    }
}
