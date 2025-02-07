use rand::*;
use tract_onnx::prelude::*;

fn main() -> TractResult<()> {
    let model = tract_onnx::onnx()
    // load the model
    .model_for_path("./example.onnx")?
    // optimize graph
    .into_optimized()?
    // make the model runnable and fix its inputs and outputs
    .into_runnable()?;

    // Generate some input data for the model
    let mut rng = thread_rng();
    let vals: Vec<_> = (0..1000).map(|_| rng.gen::<f32>()).collect();
    let input = tract_ndarray::arr1(&vals).into_shape((10, 100)).unwrap().into_tensor();

    // Input the generated data into the model
    let result = model.run(tvec![input.into()]).unwrap();
    let to_show = result[0].to_array_view::<f32>()?;
    println!("result: {:?}", to_show);
    Ok(())
}
