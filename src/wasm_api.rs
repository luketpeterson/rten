use std::borrow::Borrow;
use std::iter::zip;
use std::rc::Rc;

use rten_tensor::prelude::*;
use rten_tensor::rng::XorShiftRng;
use wasm_bindgen::prelude::*;

use crate::graph::Dimension;
use crate::model;
use crate::ops::{matmul, Input, Output};
use crate::tensor_pool::TensorPool;

#[wasm_bindgen]
pub struct Model {
    model: model::Model,
}

#[wasm_bindgen]
impl Model {
    /// Construct a new model from a serialized graph.
    #[wasm_bindgen(constructor)]
    pub fn new(model_data: Vec<u8>) -> Result<Model, String> {
        let model = model::Model::load(model_data).map_err(|e| e.to_string())?;
        Ok(Model { model })
    }

    /// Find the ID of a node in the graph from its name.
    #[wasm_bindgen(js_name = findNode)]
    pub fn find_node(&self, name: &str) -> Option<usize> {
        self.model.find_node(name)
    }

    /// Get metadata about the node with a given ID.
    ///
    /// This is useful for getting the input tensor shape expected by the model.
    #[wasm_bindgen(js_name = nodeInfo)]
    pub fn node_info(&self, id: usize) -> Option<NodeInfo> {
        self.model.node_info(id).map(|ni| NodeInfo {
            name: ni.name().map(|n| n.to_string()),
            shape: ni.shape(),
        })
    }

    /// Return the IDs of input nodes.
    ///
    /// Additional details about the nodes can be obtained using `node_info`.
    #[wasm_bindgen(js_name = inputIds)]
    pub fn input_ids(&self) -> Vec<usize> {
        self.model.input_ids().into()
    }

    /// Return the IDs of output nodes.
    ///
    /// Additional details about the nodes can be obtained using `node_info`.
    #[wasm_bindgen(js_name = outputIds)]
    pub fn output_ids(&self) -> Vec<usize> {
        self.model.output_ids().into()
    }

    /// Execute the model, passing `input` as the tensor values for the node
    /// IDs specified by `input_ids` and calculating the values of the nodes
    /// specified by `output_ids`.
    pub fn run(
        &self,
        input_ids: &[usize],
        input: Vec<Tensor>,
        output_ids: &[usize],
    ) -> Result<Vec<Tensor>, String> {
        let inputs: Vec<(usize, Input)> = zip(
            input_ids.iter().copied(),
            input.iter().map(|tensor| (&*tensor.data).into()),
        )
        .collect();
        let result = self.model.run(&inputs[..], output_ids, None);
        match result {
            Ok(outputs) => {
                let mut list = Vec::new();
                for output in outputs.into_iter() {
                    list.push(Tensor::from_output(output));
                }
                Ok(list)
            }
            Err(err) => Err(format!("{:?}", err)),
        }
    }
}

/// Metadata about a node in the model.
#[wasm_bindgen]
pub struct NodeInfo {
    name: Option<String>,
    shape: Option<Vec<Dimension>>,
}

#[wasm_bindgen]
impl NodeInfo {
    /// Returns the name of a node in the graph, if it has one.
    pub fn name(&self) -> Option<String> {
        self.name.clone()
    }

    /// Returns the tensor shape of a node in the graph.
    ///
    /// For inputs, this specifies the shape that the model expects the input
    /// to have. Dimensions can be -1 if the model does not specify a size.
    ///
    /// Note: Ideally this would return `null` for unknown dimensions, but
    /// wasm_bindgen does not support returning a `Vec<Option<i32>>`.
    pub fn shape(&self) -> Option<Vec<i32>> {
        self.shape.as_ref().map(|dims| {
            dims.iter()
                .map(|dim| match dim {
                    Dimension::Fixed(size) => *size as i32,
                    Dimension::Symbolic(_) => -1,
                })
                .collect()
        })
    }
}

/// A wrapper around a multi-dimensional array model input or output.
#[wasm_bindgen]
#[derive(Clone)]
pub struct Tensor {
    data: Rc<Output>,
}

/// Core tensor APIs needed for constructing model inputs and outputs.
#[wasm_bindgen]
impl Tensor {
    /// Construct a float tensor from the given shape and data.
    #[wasm_bindgen(js_name = floatTensor)]
    pub fn float_tensor(shape: &[usize], data: &[f32]) -> Tensor {
        let data: Output = rten_tensor::Tensor::from_data(shape, data.to_vec()).into();
        Tensor {
            data: Rc::new(data),
        }
    }

    /// Construct an int tensor from the given shape and data.
    #[wasm_bindgen(js_name = intTensor)]
    pub fn int_tensor(shape: &[usize], data: &[i32]) -> Tensor {
        let data: Output = rten_tensor::Tensor::from_data(shape, data.to_vec()).into();
        Tensor {
            data: Rc::new(data),
        }
    }

    pub fn shape(&self) -> Vec<usize> {
        self.data.shape().into()
    }

    /// Return the elements of a float tensor in their logical order.
    #[wasm_bindgen(js_name = floatData)]
    pub fn float_data(&self) -> Option<Vec<f32>> {
        match *self.data {
            Output::FloatTensor(ref t) => Some(t.to_vec()),
            _ => None,
        }
    }

    /// Return the elements of an int tensor in their logical order.
    #[wasm_bindgen(js_name = intData)]
    pub fn int_data(&self) -> Option<Vec<i32>> {
        match *self.data {
            Output::IntTensor(ref t) => Some(t.to_vec()),
            _ => None,
        }
    }

    fn from_output(out: Output) -> Tensor {
        Tensor { data: Rc::new(out) }
    }
}

/// Additional constructors and ONNX operators exposed as JS methods.
#[wasm_bindgen]
impl Tensor {
    fn as_float(&self) -> Result<rten_tensor::TensorView<f32>, String> {
        let Output::FloatTensor(ref a) = self.data.borrow() else {
            return Err("Expected a float tensor".to_string());
        };
        Ok(a.view())
    }

    /// Create a tensor filled with non-secure random numbers.
    ///
    /// `seed` specifies the seed for the random number generator. This method
    /// will always return the same output for a given seed.
    pub fn rand(shape: &[usize], seed: u64) -> Tensor {
        let mut rng = XorShiftRng::new(seed);
        let tensor = rten_tensor::Tensor::rand(shape, &mut rng);
        Tensor::from_output(tensor.into())
    }

    /// Return the matrix product of this tensor and `other`.
    ///
    /// Only float tensors are currently supported.
    ///
    /// See https://onnx.ai/onnx/operators/onnx__MatMul.html.
    pub fn matmul(&self, other: &Tensor) -> Result<Tensor, String> {
        let a = self.as_float()?;
        let b = other.as_float()?;
        let pool = TensorPool::new();
        let out = matmul(&pool, a, b).map_err(|e| e.to_string())?;
        Ok(Tensor::from_output(out.into()))
    }
}
