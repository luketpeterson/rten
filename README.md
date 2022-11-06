# Wasnn

Wasnn is a neural network inference engine for running [ONNX
models](https://onnx.ai). It has a particular focus on use in browsers and
other environments that support WebAssembly.

Wasnn is written in portable Rust and has minimal dependencies.

## Goals

 - Provide a small and reasonably efficient neural network runtime that is
   well-suited to the needs of running small models in browsers
 - Be easy to compile and run on a variety of platforms.

## Limitations

 - Only a subset of ONNX operators are currently supported.
 - There is no support for running models on the GPU or other neural network
   accelerators.
 - Wasnn is fast enough to be useful for many applications, but not as well
   optimized as more mature runtimes such as ONNX Runtime or TensorFlow
   Lite.

## Usage

See the [examples/](examples/) directory for projects that show the end-to-end steps to
use this library to run an ONNX model in the browser or Node. The [image
classification](examples/image-classification/) example is one of the simplest
and a good place to start.

Before running the examples, you will need to follow the steps under ["Building
the library"](#building-the-library) below to build the project locally. You
will also need to install the dependencies of the model conversion script,
explained under ["Preparing ONNX models"](#preparing-onnx-models).

The general steps for using Wasnn to run models in a JavaScript project are:

 1. Develop or find a pre-trained model that you want to run. Various models
    already in ONNX format are available from the [ONNX Model Zoo](https://github.com/onnx/models).
 2. Export the model in ONNX format. PyTorch users can use [torch.onnx](https://pytorch.org/docs/stable/onnx.html)
    for this.
 3. Use the `convert-onnx.py` script in this repository to convert the model
    to optimized format Wasnn uses. See the section below on preparing models.

    **Note: This library is still new.** You may run into issues where your model
    uses operators or attributes that are not supported. Please file an issue
    that includes a link to the ONNX model you want to run.

 4. In your JavaScript code, fetch the WebAssembly binary and initialize Wasnn
    using the `init` function.
 5. Fetch the prepared Wasnn model and use it to an instantiate the `Model`
    class from this library.
 6. Each time you want to run the model, prepare one or more `Float32Array`s
    containing input data in the format expected by the model, and call
    `Model.run`. This will return a `TensorList` that provides access to the
    shapes and data of the outputs.

After building the library, API documentation for the `Model` and `TensorList`
classes is available in `dist/wasnn.d.ts`.

## Preparing ONNX models

Wasnn does not load ONNX models directly. ONNX models must be run through a
conversion tool which produces an optimized model in a
[FlatBuffers](https://google.github.io/flatbuffers/)-based format that the
engine can load.

The conversion tool requires Python >= 3.10. To convert an existing ONNX model,
run:

```sh
git clone https://github.com/robertknight/wasnn.git
pip install -r wasnn/tools/requirements.txt
wasnn/tools/convert-onnx.py your-model.onnx output.model
```

The optimized Wasnn model format is not yet backwards compatible, so models
should be converted from ONNX for the specific Wasnn release that the model is
going to be used with. Typically this would be done as part of your project's
build process.

## Building the library

### Prerequisites

To build Wasnn you will need:

 - A recent stable version of Rust
 - `make`
 - (Optional) The `wasm-opt` tool from [Binaryen](https://github.com/WebAssembly/binaryen)
   can be used to optimize `.wasm` binaries for improved performance
 - (Optional) A recent version of Node for running demos

### Building wasnn

```sh
git clone https://github.com/robertknight/wasnn.git
cd wasnn
make wasm-all
```

The `make wasm-all` command will build two versions of the library, one for
browsers that support SIMD (Chrome, Firefox) and one for those which do not
(Safari <= 16). See the [WebAssembly Roadmap](https://webassembly.org/roadmap/)
for a full list of which features different engines support. The SIMD build
is significantly faster.

During development, you can speed up the testing cycle by running `make wasm`
to build only the SIMD version, or `make wasm-nosimd` for the non-SIMD version.

At runtime, you can find out which build is supported by calling the `binaryName()`
function exported by this package.