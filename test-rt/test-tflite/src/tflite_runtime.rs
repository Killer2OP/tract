use tflitec::interpreter::Interpreter;
use tflitec::model::Model;
use tflitec::tensor::DataType;

use super::*;

struct TfliteRuntime(Tflite);

impl Runtime for TfliteRuntime {
    fn name(&self) -> Cow<str> {
        "tflite".into()
    }

    fn prepare(&self, model: TypedModel) -> TractResult<Box<dyn Runnable>> {
        let mut buffer = vec![];
        self.0.write(&model, &mut buffer)?;
        let output_dt = model
            .output_outlets()?
            .iter()
            .map(|oo| model.outlet_fact(*oo).unwrap().datum_type)
            .collect();
        Ok(Box::new(TfliteRunnable(buffer, output_dt)))
    }
}

#[derive(Clone)]
struct TfliteRunnable(Vec<u8>, TVec<DatumType>);

impl Runnable for TfliteRunnable {
    fn spawn(&self) -> TractResult<Box<dyn State>> {
        Ok(Box::new(TfliteState(self.clone())))
    }
}

struct TfliteState(TfliteRunnable);

impl State for TfliteState {
    fn run(&mut self, inputs: TVec<TValue>) -> TractResult<TVec<TValue>> {
        let model = Model::from_bytes(&self.0 .0)?;
        let interpreter = Interpreter::new(&model, None)?;
        interpreter.allocate_tensors()?;
        ensure!(inputs.len() == interpreter.input_tensor_count());
        for (ix, input) in inputs.iter().enumerate() {
            let input_tensor = interpreter.input(ix)?;
            dbg!(&input_tensor);
            assert_eq!(input_tensor.shape().dimensions(), input.shape());
            dbg!(&input);
            input_tensor.set_data(unsafe { input.as_bytes() })?;
        }
        interpreter.invoke()?;
        let mut outputs = tvec![];
        for ix in 0..interpreter.output_tensor_count() {
            let output_tensor = interpreter.output(ix)?;
            let dt = match output_tensor.data_type() {
                DataType::Float32 => f32::datum_type(),
                DataType::Bool => bool::datum_type(),
                DataType::Int64 => i64::datum_type(),
                DataType::Int8 => {
                    if let Some(qp) = output_tensor.quantization_parameters() {
                        i8::datum_type().quantize(QParams::ZpScale { zero_point: qp.zero_point, scale: qp.scale })
                    } else {
                        i8::datum_type()
                    }
                }
                _ => bail!("unknown type in tract tflitec test Runtime"),
            };
            let tensor = unsafe {
                Tensor::from_raw_dt(dt, &output_tensor.shape().dimensions(), output_tensor.data())?
            };
            outputs.push(tensor.into_tvalue());
        }
        Ok(outputs)
    }
}

fn runtime() -> &'static TfliteRuntime {
    lazy_static::lazy_static! {
        static ref RT: TfliteRuntime = TfliteRuntime(Tflite::default());
    };
    &RT
}

include!(concat!(env!("OUT_DIR"), "/tests/tests.rs"));
