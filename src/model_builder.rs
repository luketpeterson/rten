extern crate flatbuffers;

use flatbuffers::{FlatBufferBuilder, UnionWIPOffset, Vector, WIPOffset};

use crate::ops::{
    AveragePool, BatchNormalization, Cast, Clip, Concat, ConstantOfShape, Conv, ConvTranspose,
    DataType, Gather, Gemm, LeakyRelu, MaxPool, Padding, ReduceMean, Resize, ResizeMode, Scalar,
    Softmax, Split, Squeeze, Transpose, Unsqueeze,
};
use crate::schema_generated as sg;
use crate::tensor::Tensor;

/// Enum of all the built-in operators
pub enum OpType {
    Add,
    AveragePool(AveragePool),
    BatchNormalization(BatchNormalization),
    Cast(Cast),
    Clip(Clip),
    Concat(Concat),
    ConstantOfShape(ConstantOfShape),
    Conv(Conv),
    ConvTranspose(ConvTranspose),
    Cos,
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
    MaxPool(MaxPool),
    Mul,
    Pad,
    Pow,
    Range,
    ReduceMean(ReduceMean),
    Relu,
    Reshape,
    Resize(Resize),
    Shape,
    Sigmoid,
    Sin,
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
    input_ids: Vec<u32>,
    output_ids: Vec<u32>,
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
            input_ids: Vec::new(),
            output_ids: Vec::new(),
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

    /// Convert a `Vec<T>` of elements to a `Vec<U>` and add them to the model buffer
    fn create_vec<'fbb, T: Copy, U: flatbuffers::Push + Copy, F: Fn(T) -> U>(
        &mut self,
        data: Option<Vec<T>>,
        map: F,
    ) -> Option<WIPOffset<Vector<'a, U::Output>>> {
        data.map(|vec| {
            let converted_vec: Vec<U> = vec.iter().copied().map(map).collect();
            self.builder.create_vector(&converted_vec)
        })
    }

    /// Add an operator node to the model
    pub fn add_operator(
        &mut self,
        id: &str,
        op_info: OpType,
        inputs: &[u32],
        outputs: &[u32],
    ) -> u32 {
        // Generate an (op_type, attr_type, attrs) tuple for an operator with
        // no attributes.
        macro_rules! op {
            ($op_name:ident) => {
                (sg::OperatorType::$op_name, sg::OperatorAttrs::NONE, None)
            };
        }

        /// Generate an (op_type, attr_type, attrs) tuple for an operator with
        /// attributes.
        macro_rules! op_with_attrs {
            ($op_name:ident, $attr_type:ident, $args: expr) => {{
                let args = ($args);
                let attrs = sg::$attr_type::create(&mut self.builder, &args).as_union_value();
                (
                    sg::OperatorType::$op_name,
                    sg::OperatorAttrs::$attr_type,
                    Some(attrs),
                )
            }};
        }

        // Convert internal operator and attribute types to corresponding
        // FlatBuffers types, and write attribute data into buffer.
        let (op_type, attrs_type, attrs) = match op_info {
            OpType::Add => op!(Add),
            OpType::AveragePool(args) => op_with_attrs!(AveragePool, AveragePoolAttrs, {
                let pad_args = pad_args_from_padding(args.padding);
                let pads = self.create_vec(pad_args.pads, |pad| pad as u32);
                let kernel_size = self.create_vec(Some(args.kernel_size.into()), |sz| sz as u32);
                let strides = self.create_vec(Some(args.strides.into()), |s| s as u32);
                sg::AveragePoolAttrsArgs {
                    kernel_size,
                    pad_mode: pad_args.pad_mode,
                    pads,
                    strides,
                }
            }),
            OpType::BatchNormalization(args) => op_with_attrs!(
                BatchNormalization,
                BatchNormalizationAttrs,
                sg::BatchNormalizationAttrsArgs {
                    epsilon: args.epsilon
                }
            ),
            OpType::Cast(args) => op_with_attrs!(
                Cast,
                CastAttrs,
                sg::CastAttrsArgs {
                    to: match args.to {
                        DataType::Int32 => sg::DataType::Int32,
                        DataType::Float => sg::DataType::Float,
                    },
                }
            ),
            OpType::Clip(args) => op_with_attrs!(
                Clip,
                ClipAttrs,
                sg::ClipAttrsArgs {
                    min: args.min,
                    max: args.max,
                }
            ),
            OpType::Concat(args) => op_with_attrs!(
                Concat,
                ConcatAttrs,
                sg::ConcatAttrsArgs {
                    dim: args.dim as u32,
                }
            ),
            OpType::ConstantOfShape(args) => {
                op_with_attrs!(ConstantOfShape, ConstantOfShapeAttrs, {
                    match args.value {
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
                    }
                })
            }
            OpType::Conv(args) => op_with_attrs!(Conv, ConvAttrs, {
                let pad_args = pad_args_from_padding(args.padding);
                let pads = self.create_vec(pad_args.pads, |pad| pad as u32);
                let strides = self.create_vec(Some(args.strides.into()), |s| s as u32);

                sg::ConvAttrsArgs {
                    groups: args.groups as u32,
                    pad_mode: pad_args.pad_mode,
                    pads,
                    strides,
                }
            }),
            OpType::ConvTranspose(args) => op_with_attrs!(ConvTranspose, ConvTransposeAttrs, {
                let strides = self.create_vec(Some(args.strides.into()), |s| s as u32);
                sg::ConvTransposeAttrsArgs { strides }
            }),
            OpType::Cos => op!(Cos),
            OpType::Div => op!(Div),
            OpType::Equal => op!(Equal),
            OpType::Erf => op!(Erf),
            OpType::Expand => op!(Expand),
            OpType::Gather(args) => op_with_attrs!(
                Gather,
                GatherAttrs,
                sg::GatherAttrsArgs {
                    axis: args.axis as u32,
                }
            ),
            OpType::Gemm(args) => op_with_attrs!(
                Gemm,
                GemmAttrs,
                sg::GemmAttrsArgs {
                    alpha: args.alpha,
                    beta: args.beta,
                    transpose_a: args.transpose_a,
                    transpose_b: args.transpose_b,
                }
            ),
            OpType::GlobalAveragePool => op!(GlobalAveragePool),
            OpType::Identity => op!(Identity),
            OpType::LeakyRelu(args) => op_with_attrs!(
                LeakyRelu,
                LeakyReluAttrs,
                sg::LeakyReluAttrsArgs { alpha: args.alpha }
            ),
            OpType::Less => op!(Less),
            OpType::MatMul => op!(MatMul),
            OpType::MaxPool(args) => op_with_attrs!(MaxPool, MaxPoolAttrs, {
                let pad_args = pad_args_from_padding(args.padding);
                let pads = self.create_vec(pad_args.pads, |pad| pad as u32);
                let kernel_size = self.create_vec(Some(args.kernel_size.into()), |sz| sz as u32);
                let strides = self.create_vec(Some(args.strides.into()), |s| s as u32);
                sg::MaxPoolAttrsArgs {
                    kernel_size,
                    pad_mode: pad_args.pad_mode,
                    pads,
                    strides,
                }
            }),
            OpType::Mul => op!(Mul),
            OpType::Pad => op!(Pad),
            OpType::Pow => op!(Pow),
            OpType::Range => op!(Range),
            OpType::ReduceMean(args) => op_with_attrs!(ReduceMean, ReduceMeanAttrs, {
                let axes = self.create_vec(args.axes, |axis| axis as i32);
                sg::ReduceMeanAttrsArgs {
                    axes,
                    keep_dims: args.keep_dims,
                }
            }),
            OpType::Relu => op!(Relu),
            OpType::Reshape => op!(Reshape),
            OpType::Resize(args) => op_with_attrs!(Resize, ResizeAttrs, {
                let mode = match args.mode {
                    ResizeMode::Nearest => sg::ResizeMode::Nearest,
                    ResizeMode::Linear => sg::ResizeMode::Linear,
                };
                sg::ResizeAttrsArgs { mode }
            }),
            OpType::Shape => op!(Shape),
            OpType::Sigmoid => op!(Sigmoid),
            OpType::Slice => op!(Slice),
            OpType::Sin => op!(Sin),
            OpType::Softmax(args) => op_with_attrs!(
                Softmax,
                SoftmaxAttrs,
                sg::SoftmaxAttrsArgs {
                    axis: args.axis as u32,
                }
            ),
            OpType::Split(args) => op_with_attrs!(Split, SplitAttrs, {
                let split = self.create_vec(Some(args.split), |size| size as u32);
                sg::SplitAttrsArgs {
                    axis: args.axis as i32,
                    split,
                }
            }),
            OpType::Sqrt => op!(Sqrt),
            OpType::Squeeze(args) => op_with_attrs!(Squeeze, SqueezeAttrs, {
                let axes = self.create_vec(args.axes, |axis| axis as u32);
                sg::SqueezeAttrsArgs { axes }
            }),
            OpType::Sub => op!(Sub),
            OpType::Transpose(args) => op_with_attrs!(Transpose, TransposeAttrs, {
                let perm = self.create_vec(args.perm, |dim| dim as u32);
                sg::TransposeAttrsArgs { perm }
            }),
            OpType::Unsqueeze(args) => op_with_attrs!(Unsqueeze, UnsqueezeAttrs, {
                let axes = self.create_vec(Some(args.axes), |axis| axis as u32);
                sg::UnsqueezeAttrsArgs { axes }
            }),
            OpType::Where => op!(Where),
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

    /// Mark a node in the graph as an input.
    pub fn add_input(&mut self, node_id: u32) {
        self.input_ids.push(node_id);
    }

    /// Mark a node in the graph as an output.
    pub fn add_output(&mut self, node_id: u32) {
        self.output_ids.push(node_id);
    }

    /// Finish writing the model data to the buffer and return the buffer's contents.
    pub fn finish(mut self) -> Vec<u8> {
        let inputs_vec = self.builder.create_vector(&self.input_ids[..]);
        let outputs_vec = self.builder.create_vector(&self.output_ids[..]);
        let nodes_vec = self.builder.create_vector(&self.nodes[..]);

        let graph = sg::Graph::create(
            &mut self.builder,
            &sg::GraphArgs {
                nodes: Some(nodes_vec),
                inputs: Some(inputs_vec),
                outputs: Some(outputs_vec),
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
