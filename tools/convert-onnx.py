#!/usr/bin/env python

import array
from argparse import ArgumentParser
from typing import Any, Literal, cast

import flatbuffers
import numpy as np
import onnx
from onnx import TensorProto
import sys

import schema_generated as sg

AttributeValue = int | float | str | list[int]


class Node:
    def __init__(self, name: str):
        self.name = name


class ConstantNode(Node):
    """
    Data for a constant value graph node.

    These are used for model weights, biases etc.
    """

    def __init__(self, name: str, shape: list[int], data: array.array):
        super().__init__(name)
        self.shape = shape
        self.data = data

    def get_scalar(self):
        if self.shape != []:
            return None
        return self.data[0]


class OperatorNode(Node):
    """
    Data for an operator graph node.
    """

    # Wasnn operator name. This should match the operator name in the FlatBuffers
    # schema.
    op_type: str

    attrs: dict[str, AttributeValue]
    inputs: list[int]
    outputs: list[int]

    def __init__(
        self,
        name: str,
        op_type: str,
        attrs: dict[str, AttributeValue],
        inputs: list[int],
        outputs: list[int],
    ):
        super().__init__(name)
        self.op_type = op_type
        self.attrs = attrs
        self.inputs = inputs
        self.outputs = outputs


class ValueNode(Node):
    """
    Data for a value placeholder graph node.

    These are used for operator inputs and outputs.
    """

    def __init__(self, name: str):
        super().__init__(name)


class Graph:
    nodes: list[Node]

    inputs: list[int]
    """Indices of nodes in `nodes` that are model inputs."""

    outputs: list[int]
    """Indices of nodes in `nodes` that are model outputs."""

    def __init__(self, nodes: list[Node], inputs: list[int], outputs: list[int]):
        self.nodes = nodes
        self.inputs = inputs
        self.outputs = outputs


# Mapping of ONNX attribute types to the field on an AttributeProto which
# contains the value. Note that if you try to access the wrong field on an
# AttributeProto, you get a default value instead of an exception.
value_fields = {
    onnx.AttributeProto.FLOAT: "f",
    onnx.AttributeProto.INT: "i",
    onnx.AttributeProto.INTS: "ints",
    onnx.AttributeProto.STRING: "s",
    onnx.AttributeProto.TENSOR: "t",
}


class ONNXOperatorReader:
    """
    Utiliy for extracting attribute and input values from an ONNX operator.

    This keeps track of which attributes have been read so that we can warn about
    any unhandled ones.
    """

    onnx_op: onnx.OperatorProto

    _handled_attrs: set[str]
    """Names of attributes that have been handled."""

    def __init__(self, onnx_op: onnx.OperatorProto):
        self.onnx_op = onnx_op

        self._handled_attrs = set()

    def get_attr(self, name: str, expected_type: str, default):
        """Get the value of an optional operator attribute."""

        self._handled_attrs.add(name)

        type_code = getattr(onnx.AttributeProto, expected_type.upper())
        for attr in self.onnx_op.attribute:
            if attr.name == name:
                if attr.type != type_code:
                    raise Exception(
                        f"Attribute {name} type does not match {expected_type}"
                    )
                val = getattr(attr, value_fields[type_code])

                # String attribute values are stored as bytes, so we have to decode
                # them.
                if expected_type == "string":
                    val = val.decode()

                return val
        return default

    def ignore_attr(self, name: str):
        """
        Mark an attribute as ignored.

        This is useful in cases where an attribute contains redundant information.
        """
        self._handled_attrs.add(name)

    def require_attr(self, name: str, expected_type: str):
        """Get the value of a required operator attribute."""
        val = self.get_attr(name, expected_type, default=None)
        if val is None:
            raise Exception(f"Missing required attribute {name}")
        return val

    def require_attr_or_input(
        self,
        name: str,
        expected_type: str,
        input_index: int,
        constant_nodes: dict[str, ConstantNode],
    ):
        """
        Get the value of a required operator attribute or input.

        Some operator inputs changed from attributes to inputs in different ONNX
        releases. This function will look up the value for the input from both
        possible sources.

        In the case where the value comes from an input, it must be a constant
        (ie. specified via an initializer or Constant node in the graph), rather
        than a value computed at runtime.

        :param name: The name of the attribute
        :param expected_type: The required type of the value
        :param input_index: The index of the operator input
        :param constant_nodes: Map of all the constant values in the model
        """
        val = self.get_attr(name, expected_type, None)
        if val is None and len(self.onnx_op.input) > input_index:
            input_val = constant_nodes.get(self.onnx_op.input[input_index])
            if input_val is None:
                raise Exception(f'Input node nor found or not a constant for "{name}"')

            # This function currently only supports extracting scalars from
            # constants, but we could also supports lists as well here.
            scalar = input_val.get_scalar()
            if scalar is None:
                raise Exception(f'Input for "{name}" is not a scalar')
            return scalar

        if val is None:
            raise Exception(f'Missing required attribute or input "{name}"')
        return val

    def check_attr(self, name: str, expected_type, default):
        """Check if an operator has an unsupported non-default value for an attribute."""
        val = self.get_attr(name, expected_type, default)
        if val != default:
            raise Exception(
                f"Unsupported value {val} for attribute {name}. Default is {default}"
            )

    def unhandled_attrs(self) -> list[onnx.AttributeProto]:
        """Return a list of attributes which have not been read."""
        return [
            attr
            for attr in self.onnx_op.attribute
            if attr.name not in self._handled_attrs
        ]


def check_ints_length(name: str, ints: list[int], allowed_length: int):
    """
    Check that an ints attribute has a fixed length.

    Various ONNX operators allow for a wider range of dimensions and per-axis
    values (eg. for strides, dilations, padding...) than this library currently
    supports.
    """
    if len(ints) != allowed_length:
        raise Exception(f'Attribute "{name}" must have {allowed_length} values')


def convert_array(src_type: str, data: bytes, dest_type: str):
    converted = [x for x in array.array(src_type, data)]
    try:
        return array.array(dest_type, converted)
    except OverflowError:
        # Some ONNX exporters use `INT_MIN` and `INT_MAX` to represent infinity
        # in certain cases, for example slicing to the end of a dimension with
        # unknown size (see
        # https://github.com/onnx/onnx/blob/main/docs/Operators.md#slice and
        # https://github.com/pytorch/pytorch/issues/17606).
        #
        # In the case where the value is an `int64` and we are converting this
        # to an `int32` in the model, this will cause an overflow. To resolve
        # this, clamp the value to the min/max values for the smaller integer
        # type we are using.
        MAX_INT = 2**31 - 1
        MIN_INT = -(2**31) + 1

        saturated = []

        for x in converted:
            if x > MAX_INT:
                print(f"Clamping out-of-range tensor value {x} to {MAX_INT}")
                x = MAX_INT
            elif x < MIN_INT:
                print(f"Clamping out-of-range tensor value {x} to {MIN_INT}")
                x = MIN_INT
            saturated.append(x)

        return array.array(dest_type, saturated)


def constant_node_from_onnx_initializer(tensor) -> ConstantNode:
    dims = list(tensor.dims)

    # Tensors can either store data in a type-appropriate field, or the `raw_data`
    # field. Only one of these should be set.
    tensor_data = (
        tensor.float_data or tensor.int64_data or tensor.int32_data or tensor.raw_data
    )

    # Convert the tensor data to a format supported by this library. For int64
    # tensors, we convert them to int32 and just ignore any issues with
    # overflows.
    match tensor.data_type:
        case onnx.TensorProto.FLOAT:
            data = array.array("f", tensor_data)
        case onnx.TensorProto.UINT8:
            data = convert_array("B", tensor_data, "i")
        case onnx.TensorProto.INT8:
            data = convert_array("b", tensor_data, "i")
        case onnx.TensorProto.UINT16:
            data = convert_array("H", tensor_data, "i")
        case onnx.TensorProto.INT16:
            data = convert_array("h", tensor_data, "i")
        case onnx.TensorProto.INT32:
            data = array.array("i", tensor_data)
        case onnx.TensorProto.INT64:
            data = convert_array("q", tensor_data, "i")
        case _:
            raise ValueError(f"Unsupported tensor data type {tensor.data_type}")

    return ConstantNode(name=tensor.name, shape=dims, data=data)


def constant_node_from_onnx_constant_op(onnx_op: onnx.OperatorProto) -> ConstantNode:
    tensor = ONNXOperatorReader(onnx_op).require_attr("value", "tensor")
    const_node = constant_node_from_onnx_initializer(tensor)

    if not len(onnx_op.output):
        raise Exception(f'Operator "{onnx_op.name}" has no outputs')
    const_node.name = onnx_op.output[0]

    return const_node


def value_node_from_onnx_value(value: onnx.ValueInfoProto) -> ValueNode:
    return ValueNode(name=value.name)


def read_pads(op_reader: ONNXOperatorReader, attrs: dict[str, AttributeValue]):
    """
    Read a padding specification from an ONNX operator.
    """

    auto_pad = op_reader.get_attr("auto_pad", "string", "NOTSET")

    match auto_pad:
        case "SAME_UPPER" | "SAME_LOWER":
            attrs["pad_mode"] = "same"
        case "NOTSET":
            padding = op_reader.get_attr("pads", "ints", [0, 0, 0, 0])
            if len(padding) != 4:
                raise Exception('"padding" attribute must have 4 values')
            pad_top, pad_left, pad_right, pad_bottom = iter(padding)

            attrs["pad_mode"] = "fixed"
            attrs["pads"] = [pad_top, pad_left, pad_bottom, pad_right]
        case other:
            raise Exception(f"Unsupported auto_pad value {other}")


def read_stride(
    op_reader: ONNXOperatorReader,
    attrs: dict[str, AttributeValue],
    require_uniform: bool,
):
    """
    Read a stride specification from an ONNX operator.
    """
    strides = op_reader.get_attr("strides", "ints", [1, 1])
    if len(strides) != 2:
        raise Exception('"strides" attribute must have 2 values')
    stride_width, stride_height = iter(strides)
    if require_uniform:
        if stride_width != stride_height:
            raise Exception("Strides must be the same in all dimensions")
        attrs["stride"] = stride_width
    else:
        attrs["stride"] = [stride_width, stride_height]


# Set of operators that have no attributes.
#
# Some of these ops *do* have attributes in the ONNX version, but those
# attributes are not yet supported in Wasnn.
NO_ATTR_OPS = {
    "Add",
    "Div",
    "Equal",
    "Erf",
    "Expand",
    "GlobalAveragePool",
    "Identity",
    "Less",
    "MatMul",
    "Mul",
    "Pad",
    "Pow",
    "Range",
    "Relu",
    "Reshape",
    "Shape",
    "Sigmoid",
    "Slice",
    "Sqrt",
    "Sub",
    "Where",
}


def op_node_from_onnx_operator(
    onnx_op: onnx.OperatorProto,
    node_index_from_name: dict[str, int],
    constant_nodes: dict[str, ConstantNode],
) -> OperatorNode:
    """
    Map an ONNX operator to the equivalent operator in this library.

    See https://github.com/onnx/onnx/blob/main/docs/Operators.md for list of
    available ONNX operators and attributes for each.
    """
    input_indexes = []
    for input_name in onnx_op.input:
        index = node_index_from_name.get(input_name)
        if index is None:
            raise Exception(
                f'Unable to find input "{input_name}" for operator {onnx_op.name}'
            )
        input_indexes.append(index)

    output_indexes = []
    for output_name in onnx_op.output:
        index = node_index_from_name.get(output_name)
        if index is None:
            raise Exception(
                f'Unable to find output "{output_name}" for operator {onnx_op.name}'
            )
        output_indexes.append(index)

    attrs: dict[str, AttributeValue] = {}

    # Operator type name in Wasnn models. By default assume this is the same as
    # the ONNX type.
    op_type = onnx_op.op_type

    op_reader = ONNXOperatorReader(onnx_op)

    match onnx_op.op_type:
        case "AveragePool":
            kernel_shape = op_reader.require_attr("kernel_shape", "ints")
            check_ints_length("kernel_shape", kernel_shape, 2)
            attrs["kernel_size"] = kernel_shape

            read_pads(op_reader, attrs)
            read_stride(op_reader, attrs, require_uniform=False)

            op_reader.check_attr("ceil_mode", "int", 0)
            op_reader.check_attr("count_include_pad", "int", 0)

        case "BatchNormalization":
            attrs["epsilon"] = op_reader.get_attr("epsilon", "float", 1e-5)

        case "Cast":
            to = op_reader.get_attr("to", "int", TensorProto.DataType.FLOAT)
            match to:
                case TensorProto.DataType.FLOAT:
                    attrs["to"] = sg.DataType.Float
                case TensorProto.DataType.BOOL | TensorProto.DataType.INT32 | TensorProto.DataType.INT64:
                    attrs["to"] = sg.DataType.Int32
                case _:
                    raise Exception(f"Unsupported target type for cast {to}")

        case "Clip":
            attrs["min"] = op_reader.require_attr_or_input(
                "min", "float", 1, constant_nodes
            )
            attrs["max"] = op_reader.require_attr_or_input(
                "max", "float", 2, constant_nodes
            )

        case "Concat":
            attrs["dim"] = op_reader.require_attr("axis", "int")

        case "ConstantOfShape":
            tensor = op_reader.require_attr("value", "tensor")
            const_node = constant_node_from_onnx_initializer(tensor)

            if len(const_node.data) != 1:
                raise Exception(
                    "Expected ConstantOfShape value to be a 1-element tensor"
                )

            attrs["value"] = const_node.data[0]

        case "Conv":
            attrs["groups"] = op_reader.get_attr("group", "int", 1)
            read_pads(op_reader, attrs)
            read_stride(op_reader, attrs, require_uniform=True)

            op_reader.check_attr("dilations", "ints", [1, 1])

            # The kernel shape is inferred at runtime from the input weight tensor.
            op_reader.ignore_attr("kernel_shape")

        case "ConvTranspose":
            read_stride(op_reader, attrs, require_uniform=True)

            op_reader.check_attr("auto_pad", "string", "NOTSET")
            op_reader.check_attr("dilations", "ints", [1, 1])
            op_reader.check_attr("group", "int", 1)

            # The kernel shape is inferred at runtime from the input weight tensor.
            op_reader.ignore_attr("kernel_shape")

            op_reader.check_attr("output_padding", "ints", [0, 0, 0, 0])
            op_reader.check_attr("pads", "ints", [0, 0, 0, 0])

        case "Gather":
            attrs["axis"] = op_reader.get_attr("axis", "int", 0)

        case "Gemm":
            attrs["alpha"] = op_reader.get_attr("alpha", "float", 1.0)
            attrs["beta"] = op_reader.get_attr("beta", "float", 1.0)
            attrs["transpose_a"] = bool(op_reader.get_attr("transA", "int", 0))
            attrs["transpose_b"] = bool(op_reader.get_attr("transB", "int", 0))

        case "LeakyRelu":
            attrs["alpha"] = op_reader.get_attr("alpha", "float", 0.01)

        case "MaxPool":
            kernel_shape = op_reader.require_attr("kernel_shape", "ints")
            check_ints_length("kernel_shape", kernel_shape, 2)
            attrs["kernel_size"] = kernel_shape

            read_pads(op_reader, attrs)
            read_stride(op_reader, attrs, require_uniform=False)

            op_reader.check_attr("ceil_mode", "int", 0)
            op_reader.check_attr("dilations", "ints", [1, 1])
            op_reader.check_attr("storage_order", "int", 0)

        case "ReduceMean":
            attrs["axes"] = op_reader.get_attr("axes", "ints", None)
            attrs["keep_dims"] = bool(op_reader.get_attr("keepdims", "int", 1))

            op_reader.check_attr("noop_with_empty_axes", "int", 0)

        case "Reshape":
            op_reader.check_attr("allowzero", "int", 0)

        case "Resize":
            attrs["mode"] = op_reader.get_attr("mode", "string", "nearest")

            op_reader.check_attr("antialias", "int", 0)

            # We only support resizing HW dimensions of NCHW tensor
            op_reader.check_attr("axes", "ints", [2, 3])

            op_reader.check_attr(
                "coordinate_transformation_mode",
                "string",
                "half_pixel",
            )
            op_reader.check_attr("cubic_coeff_a", "float", -0.75)
            op_reader.check_attr("exclude_outside", "int", 0)
            op_reader.check_attr("extrapolation_value", "float", 0.0)
            op_reader.check_attr("keep_aspect_ratio_policy", "string", "stretch")
            op_reader.check_attr("nearest_mode", "string", "prefer_round_floor")

        case "Pad":
            op_reader.check_attr("mode", "string", "constant")

        case "Shape":
            op_reader.check_attr("end", "int", 0)
            op_reader.check_attr("start", "int", 0)

        case "Softmax":
            attrs["axis"] = op_reader.get_attr("axis", "int", 0)

        case "Split":
            attrs["axis"] = op_reader.get_attr("axis", "int", 0)
            attrs["split"] = op_reader.get_attr("split", "ints", [])

            op_reader.check_attr("num_outputs", "int", 0)

        case "Squeeze":
            axes = op_reader.get_attr("axes", "ints", [])
            attrs["axes"] = axes

        case "Transpose":
            perm = op_reader.get_attr("perm", "ints", [])
            attrs["perm"] = perm

        case "Unsqueeze":
            axes = op_reader.get_attr("axes", "ints", [])
            attrs["axes"] = axes

        case other_type:
            if other_type not in NO_ATTR_OPS:
                raise Exception(f"Unsupported operation {onnx_op.op_type}")

    # Display a warning for any attributes that were not handled above.
    for attr in op_reader.unhandled_attrs():
        print(
            f"WARNING: Unsupported attribute {attr.name} for operator {onnx_op.op_type}",
            file=sys.stderr,
        )

    return OperatorNode(
        name=onnx_op.name,
        op_type=op_type,
        attrs=attrs,
        inputs=input_indexes,
        outputs=output_indexes,
    )


def graph_from_onnx_graph(onnx_graph: onnx.GraphProto) -> Graph:
    """
    Parse an ONNX model into a graph representation compatible with this library.
    """
    nodes: list[Node] = []

    # Map from tensor ID to node index
    tensor_map: dict[str, int] = {}

    # Map of constant/initializer name to node
    constant_map: dict[str, ConstantNode] = {}

    def add_node(node: Node):
        if node.name in tensor_map:
            raise Exception(f'Node name "{node.name}" conflicts with another node')
        if isinstance(node, ConstantNode):
            constant_map[node.name] = node
        nodes.append(node)
        tensor_map[node.name] = len(nodes) - 1

    for tensor in onnx_graph.initializer:
        const_node = constant_node_from_onnx_initializer(tensor)
        add_node(const_node)
    for operator in onnx_graph.node:
        if operator.op_type != "Constant":
            continue
        const_node = constant_node_from_onnx_constant_op(operator)
        add_node(const_node)

    for value in onnx_graph.input:
        # If the same node is referenced in the ONNX model's `initializer` and
        # `input` properties, ignore the one from the input.
        if value.name in tensor_map:
            continue
        value_node = value_node_from_onnx_value(value)
        add_node(value_node)

    for operator in onnx_graph.node:
        if operator.op_type == "Constant":
            continue

        for output_name in operator.output:
            value_node = ValueNode(output_name)
            add_node(value_node)

        op_node = op_node_from_onnx_operator(operator, tensor_map, constant_map)
        add_node(op_node)

    inputs = [tensor_map[info.name] for info in onnx_graph.input]
    outputs = [tensor_map[info.name] for info in onnx_graph.output]
    return Graph(nodes=nodes, inputs=inputs, outputs=outputs)


def build_constant_node(builder: flatbuffers.Builder, constant: ConstantNode):
    """
    Serialize a constant tensor value (eg. model weights) into a FlatBuffers model.
    """
    shape_vec = write_vec(
        builder, sg.ConstantNodeStartShapeVector, constant.shape, "u32"
    )

    # Convert data to NumPy array then serialize. This is much faster than
    # serializing a Python array element by element.
    data_vec = builder.CreateNumpyVector(np.array(constant.data))

    match constant.data.typecode:
        case "f":
            sg.FloatDataStart(builder)
            sg.FloatDataAddData(builder, data_vec)
            const_data = sg.FloatDataEnd(builder)
            const_data_type = sg.ConstantData.FloatData
        case "i":
            sg.IntDataStart(builder)
            sg.IntDataAddData(builder, data_vec)
            const_data = sg.IntDataEnd(builder)
            const_data_type = sg.ConstantData.IntData
        case _:
            raise ValueError(f"Unsupported data array type {constant.data.typecode}")

    sg.ConstantNodeStart(builder)
    sg.ConstantNodeAddShape(builder, shape_vec)
    sg.ConstantNodeAddDataType(builder, const_data_type)
    sg.ConstantNodeAddData(builder, const_data)
    return sg.ConstantNodeEnd(builder)


def write_vec(
    builder: flatbuffers.Builder,
    start_vec,
    data: list[int],
    dtype: Literal["u32", "i32", "offset"],
):
    """
    Serialize a list into a vector in a FlatBuffers buffer.

    `start_vec` is the generated function that starts the vector.
    """
    start_vec(builder, len(data))
    for item in reversed(data):
        match dtype:
            case "u32":
                builder.PrependUint32(item)
            case "i32":
                builder.PrependInt32(item)
            case "offset":
                builder.PrependUOffsetTRelative(item)
            case _:
                raise ValueError("Unsupported data type")
    return builder.EndVector()


def build_operator_node(builder: flatbuffers.Builder, operator: OperatorNode):
    """
    Serialize an operator into a FlatBuffers model.
    """
    if operator.op_type in NO_ATTR_OPS:
        attrs_type = sg.OperatorAttrs.NONE
    else:
        attrs_type = getattr(sg.OperatorAttrs, operator.op_type + "Attrs")
    attrs = None

    match operator.op_type:
        case "AveragePool":
            if operator.attrs["pad_mode"] == "same":
                pad_mode = sg.PadMode.Same
            else:
                pad_mode = sg.PadMode.Fixed
            attrs = sg.AveragePoolAttrsT()
            attrs.kernelSize = cast(list[int], operator.attrs["kernel_size"])
            attrs.padMode = pad_mode
            attrs.pads = cast(list[int], operator.attrs.get("pads"))
            attrs.stride = cast(list[int], operator.attrs["stride"])

        case "BatchNormalization":
            attrs = sg.BatchNormalizationAttrsT()
            attrs.epsilon = cast(float, operator.attrs["epsilon"])

        case "Cast":
            attrs = sg.CastAttrsT()
            attrs.to = cast(int, operator.attrs["to"])

        case "Clip":
            attrs = sg.ClipAttrsT()
            attrs.min = cast(float, operator.attrs["min"])
            attrs.max = cast(float, operator.attrs["max"])

        case "Concat":
            attrs = sg.ConcatAttrsT()
            attrs.dim = cast(int, operator.attrs["dim"])

        case "ConstantOfShape":
            value = operator.attrs["value"]

            if isinstance(value, float):
                scalar_type = sg.Scalar.FloatScalar
                scalar = sg.FloatScalarT()
                scalar.value = value
            elif isinstance(value, int):
                scalar_type = sg.Scalar.IntScalar
                scalar = sg.IntScalarT()
                scalar.value = value
            else:
                raise ValueError(
                    f"Unsupported value type {type(value)} for ConstantOfShape"
                )

            attrs = sg.ConstantOfShapeAttrsT()
            attrs.valueType = scalar_type
            attrs.value = scalar

        case "Conv":
            attrs = sg.ConvAttrsT()
            if operator.attrs["pad_mode"] == "same":
                attrs.padMode = sg.PadMode.Same
            else:
                attrs.padMode = sg.PadMode.Fixed
                attrs.pads = cast(list[int], operator.attrs["pads"])
            attrs.groups = cast(int, operator.attrs["groups"])
            attrs.stride = cast(int, operator.attrs["stride"])

        case "ConvTranspose":
            attrs = sg.ConvTransposeAttrsT()
            attrs.stride = cast(int, operator.attrs["stride"])

        case "Gather":
            attrs = sg.GatherAttrsT()
            attrs.axis = cast(int, operator.attrs["axis"])

        case "Gemm":
            attrs = sg.GemmAttrsT()
            attrs.alpha = cast(float, operator.attrs["alpha"])
            attrs.beta = cast(float, operator.attrs["beta"])
            attrs.transposeA = cast(bool, operator.attrs["transpose_a"])
            attrs.transposeB = cast(bool, operator.attrs["transpose_b"])

        case "LeakyRelu":
            attrs = sg.LeakyReluAttrsT()
            attrs.alpha = cast(float, operator.attrs["alpha"])

        case "MaxPool":
            attrs = sg.MaxPoolAttrsT()
            if operator.attrs["pad_mode"] == "same":
                attrs.padMode = sg.PadMode.Same
            else:
                attrs.padMode = sg.PadMode.Fixed
                attrs.pads = cast(list[int], operator.attrs["pads"])

            attrs.kernelSize = cast(list[int], operator.attrs["kernel_size"])
            if "stride" in operator.attrs:
                attrs.stride = cast(list[int], operator.attrs["stride"])

        case "ReduceMean":
            attrs = sg.ReduceMeanAttrsT()
            attrs.keepDims = cast(bool, operator.attrs["keep_dims"])
            if operator.attrs["axes"]:
                attrs.axes = cast(list[int], operator.attrs["axes"])

        case "Resize":
            if operator.attrs["mode"] == "nearest":
                mode = sg.ResizeMode.Nearest
            elif operator.attrs["mode"] == "linear":
                mode = sg.ResizeMode.Linear
            else:
                raise ValueError(f"Unsupported resize mode {operator.attrs['mode']}")
            attrs = sg.ResizeAttrsT()
            attrs.mode = mode

        case "Softmax":
            attrs = sg.SoftmaxAttrsT()
            attrs.axis = cast(int, operator.attrs["axis"])

        case "Split":
            attrs = sg.SplitAttrsT()
            attrs.axis = cast(int, operator.attrs["axis"])
            if operator.attrs["split"]:
                attrs.split = cast(list[int], operator.attrs["split"])

        case "Squeeze":
            attrs = sg.SqueezeAttrsT()
            if operator.attrs["axes"]:
                attrs.axes = cast(list[int], operator.attrs["axes"])

        case "Transpose":
            attrs = sg.TransposeAttrsT()
            if operator.attrs["perm"]:
                attrs.perm = cast(list[int], operator.attrs["perm"])

        case "Unsqueeze":
            attrs = sg.UnsqueezeAttrsT()
            attrs.axes = cast(list[int], operator.attrs["axes"])

        case other:
            if operator.op_type not in NO_ATTR_OPS:
                raise Exception(f"Unsupported operator type {operator.op_type}")

    operator_table = sg.OperatorNodeT()
    operator_table.type = getattr(sg.OperatorType, operator.op_type)
    operator_table.attrsType = attrs_type
    operator_table.attrs = attrs
    operator_table.inputs = operator.inputs
    operator_table.outputs = operator.outputs
    return operator_table.Pack(builder)


def build_value_node(builder: flatbuffers.Builder, value: ValueNode):
    """
    Serialize a placeholder for an input/output value into a FlatBuffers model.
    """
    sg.ValueNodeStart(builder)
    return sg.ValueNodeEnd(builder)


def write_graph(graph: Graph, out_path: str):
    """
    Serialize a model graph into a flatbuffers model.

    This serializes the parsed graph representation into the flatbuffers-based
    model format that this library uses.
    """

    builder = flatbuffers.Builder(initialSize=1024)

    node_offsets = []
    for node in graph.nodes:
        match node:
            case ConstantNode():
                data_type = sg.NodeKind.ConstantNode
                data = build_constant_node(builder, node)
            case OperatorNode():
                data_type = sg.NodeKind.OperatorNode
                data = build_operator_node(builder, node)
            case ValueNode():
                data_type = sg.NodeKind.ValueNode
                data = build_value_node(builder, node)
            case _:
                raise Exception("Unsupported node type")

        name_str = builder.CreateString(node.name)
        sg.NodeStart(builder)
        sg.NodeAddName(builder, name_str)
        sg.NodeAddDataType(builder, data_type)
        sg.NodeAddData(builder, data)
        node_offset = sg.NodeEnd(builder)
        node_offsets.append(node_offset)

    graph_nodes = write_vec(builder, sg.GraphStartNodesVector, node_offsets, "offset")
    inputs = write_vec(builder, sg.GraphStartInputsVector, graph.inputs, "u32")
    outputs = write_vec(builder, sg.GraphStartOutputsVector, graph.outputs, "u32")

    sg.GraphStart(builder)
    sg.GraphAddNodes(builder, graph_nodes)
    sg.GraphAddInputs(builder, inputs)
    sg.GraphAddOutputs(builder, outputs)
    graph = sg.GraphEnd(builder)

    sg.ModelStart(builder)
    sg.ModelAddSchemaVersion(builder, 1)
    sg.ModelAddGraph(builder, graph)
    model = sg.ModelEnd(builder)

    builder.Finish(model)
    data = builder.Output()

    with open(out_path, "wb") as output:
        output.write(data)


def main():
    parser = ArgumentParser()
    parser.add_argument("model", help="Input ONNX model")
    parser.add_argument("out_name", help="Output model file")
    args = parser.parse_args()

    model_path = args.model

    model = onnx.load(model_path)
    graph = graph_from_onnx_graph(model.graph)
    write_graph(graph, args.out_name)


if __name__ == "__main__":
    main()
