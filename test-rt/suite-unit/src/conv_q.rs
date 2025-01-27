use infra::{Test, TestSuite};
use proptest::collection::vec;
use proptest::prelude::*;
// use proptest::*;
use tract_core::internal::*;
use tract_core::ops::cnn::KernelFormat::*;
use tract_core::ops::cnn::{ConvUnary, KernelFormat, PaddingSpec, PoolSpec};
use tract_core::ops::math::round_ties_to_even;
use tract_core::ops::nn::DataFormat::*;
use tract_core::ops::nn::DataShape;
use tract_core::tract_data::itertools::Itertools;
use tract_itertools::izip;
use tract_ndarray::*;

use crate::conv_f32::ConvProblemParams;

pub fn qtensor(shape: Vec<usize>) -> BoxedStrategy<ArrayD<i8>> {
    let len = shape.iter().product::<usize>();
    vec(any::<i8>(), len..=len)
        .prop_map(move |vec| ArrayD::from_shape_vec(shape.clone(), vec).unwrap())
        .boxed()
}
/* https://www.tensorflow.org/lite/performance/quantization_spec
CONV_2D
Input 0:
data_type  : int8
range      : [-128, 127]
granularity: per-tensor
Input 1 (Weight):
data_type  : int8
range      : [-127, 127]
granularity: per-axis (dim = 0)
restriction: zero_point = 0
Input 2 (Bias):
data_type  : int32
range      : [int32_min, int32_max]
granularity: per-axis
restriction: (scale, zero_point) = (input0_scale * input1_scale[...], 0)
Output 0:
data_type  : int8
range      : [-128, 127]
granularity: per-tensor
*/

#[allow(clippy::arc_with_non_send_sync)]
pub fn q_params(params: &QConvProblemParams, co: usize) -> BoxedStrategy<[Tensor; 6]> {
    let a0 = if params.no_kernel_zero_point { Just(0i32).boxed() } else { (-10..10i32).boxed() };
    (
        a0,
        -10i32..10,
        -10i32..10,
        prop_oneof![
            (Just(false), (-3..3i32).prop_map(|x| vec!(x)).boxed()),
            (Just(true), vec(-3..3i32, co..=co).boxed())
        ],
        -3..3i32,
        -3..3i32,
    )
        .prop_map(|(a0, b0, c0, a_scale, b_scale, c_scale)| {
            let a_scale_values = a_scale.1.iter().map(|x| 2f32.powi(*x)).collect_vec();
            [
                tensor0(a0),
                if a_scale.0 { tensor1(&a_scale_values) } else { tensor0(a_scale_values[0]) },
                tensor0(b0),
                tensor0(2f32.powi(b_scale)),
                tensor0(c0),
                tensor0(2f32.powi(c_scale)),
            ]
        })
        .boxed()
}

#[derive(Debug, Clone, Default)]
pub struct QConvProblemParams {
    pub conv: ConvProblemParams,
    pub no_kernel_zero_point: bool,
}

#[derive(Debug, Clone)]
pub struct QConvProblem {
    shape_in: DataShape,
    kernel_format: KernelFormat,
    co: usize,
    group: usize,
    data: ArrayD<i8>,
    kernel: ArrayD<i8>,
    bias: Option<ArrayD<i32>>,
    qp: [Tensor; 6],
}

impl QConvProblem {
    fn geo_ker(&self) -> &[usize] {
        &self.kernel.shape()[self.kernel_format.h_axis()..][..self.shape_in.hw_rank()]
    }

    fn reference(&self) -> Tensor {
        assert_eq!(self.data.shape(), &*self.shape_in.shape);
        let n = *self.shape_in.n().unwrap_or(&1);
        let ci_per_g = self.shape_in.c() / self.group;
        let co_per_g = self.co / self.group;
        let a0 = self.qp[0].cast_to_scalar::<i32>().unwrap();
        let b0 = self.qp[2].cast_to_scalar::<i32>().unwrap();
        let c0 = self.qp[4].cast_to_scalar::<i32>().unwrap();
        let b_scale = self.qp[3].cast_to_scalar::<f32>().unwrap();
        let c_scale = self.qp[5].cast_to_scalar::<f32>().unwrap();
        let shape_out: TVec<usize> = izip!(self.shape_in.hw_dims(), self.geo_ker())
            .map(|(i, k)| (*i + 1).saturating_sub(*k))
            .collect();
        let shape_out = self
            .shape_in
            .fmt
            .from_n_c_hw(self.shape_in.n().cloned().unwrap_or(1), co_per_g * self.group, shape_out)
            .unwrap();
        // a is the kernel, it can be quantized per O axis
        let a_scale = if self.qp[1].len() == 1 {
            vec![self.qp[1].cast_to_scalar::<f32>().unwrap(); *shape_out.c()]
        } else {
            self.qp[1].as_slice::<f32>().unwrap().into()
        };
        let mut temp = ArrayD::<i32>::zeros(&*shape_out.shape);
        for n in 0..n {
            for g in 0..self.group {
                for geo_out in tract_ndarray::indices(shape_out.hw_dims()) {
                    let mut output_coords: TVec<usize> = geo_out.slice().into();
                    if self.shape_in.fmt.has_n() {
                        output_coords.insert(0, n);
                    }
                    output_coords.insert(shape_out.c_axis(), 0);
                    for geo_ker in tract_ndarray::indices(self.geo_ker()) {
                        let mut input_coords: TVec<usize> =
                            izip!(geo_out.slice(), geo_ker.slice()).map(|(a, b)| a + b).collect();
                        if self.shape_in.fmt.has_n() {
                            input_coords.insert(0, n);
                        }
                        input_coords.insert(self.shape_in.c_axis(), 0);
                        for ci in 0..ci_per_g {
                            input_coords[self.shape_in.c_axis()] = ci + g * ci_per_g;
                            let i = self.data[&*input_coords] as i32;
                            for co in 0..co_per_g {
                                output_coords[shape_out.c_axis()] = co + g * co_per_g;
                                let mut kernel_coords: TVec<usize> = geo_ker.slice().into();
                                match self.kernel_format {
                                    KernelFormat::OIHW => {
                                        kernel_coords.insert(0, ci);
                                        kernel_coords.insert(0, co + g * co_per_g);
                                    }
                                    KernelFormat::HWIO => {
                                        kernel_coords.push(ci + g * ci_per_g);
                                        kernel_coords.push(co);
                                    }
                                    KernelFormat::OHWI => {
                                        kernel_coords.insert(0, co);
                                        kernel_coords.push(ci + g * ci_per_g);
                                    }
                                }
                                let k = self.kernel[&*kernel_coords] as i32;
                                temp[&*output_coords] += (k - a0) * (i - b0);
                            }
                        }
                    }
                }
            }
        }
        if let Some(bias) = &self.bias {
            let mut shape = vec![1; temp.ndim()];
            shape[shape_out.c_axis()] = bias.len();
            temp += &bias.clone().into_shape(shape).unwrap();
        }
        temp.axis_iter_mut(Axis(shape_out.c_axis())).zip(a_scale).for_each(
            |(mut view, a_scale)| {
                view.mapv_inplace(|i| {
                    (round_ties_to_even(i as f32 / c_scale * a_scale * b_scale) as i32 + c0)
                        .max(std::i8::MIN as i32)
                        .min(std::i8::MAX as i32)
                })
            },
        );
        temp.into_tensor()
            .cast_to_dt(
                i8::datum_type().quantize(QParams::ZpScale { zero_point: c0, scale: c_scale }),
            )
            .unwrap()
            .into_owned()
    }

    fn tract(&self) -> TractResult<TypedModel> {
        assert!(self.data.shape() == &*self.shape_in.shape);
        let mut model = TypedModel::default();
        let kdt = DatumType::QI8(QParams::ZpScale {
            zero_point: self.qp[0].cast_to_scalar()?,
            scale: *self.qp[1].to_scalar()?,
        });
        let idt = DatumType::QI8(QParams::ZpScale {
            zero_point: self.qp[2].cast_to_scalar()?,
            scale: *self.qp[3].to_scalar()?,
        });
        let cdt = DatumType::QI8(QParams::ZpScale {
            zero_point: self.qp[4].cast_to_scalar()?,
            scale: *self.qp[5].to_scalar()?,
        });
        let wire = model.add_source("input", idt.fact(&self.shape_in.shape))?;
        let mut inputs = tvec!(wire);
        for (ix, qp) in self.qp.iter().enumerate() {
            inputs.push(model.add_const(format!("qp.{ix}"), qp.clone())?);
        }
        let mut kernel = self.kernel.clone().into_tensor();
        unsafe { kernel.set_datum_type(kdt) };
        let op = ConvUnary::new(
            PoolSpec::new(
                self.shape_in.fmt,
                self.geo_ker().into(),
                PaddingSpec::Valid,
                None,
                None,
                Some(self.co),
            ),
            self.kernel_format,
            kernel.into_arc_tensor(),
            self.group,
            self.bias.clone().map(|a| a.into_arc_tensor()),
            Some(cdt),
        );
        let wire = model.wire_node("conv", op, &inputs)?[0];
        model.set_output_outlets(&[wire])?;
        Ok(model)
    }
}

impl Test for QConvProblem {
    fn run_with_approx(
        &self,
        id: &str,
        runtime: &dyn Runtime,
        approx: Approximation,
    ) -> infra::TestResult {
        let reference = self.reference();
        let mut model = self.tract()?;
        model.properties.insert("tract-rt-test.id".to_string(), rctensor0(id.to_string()));
        let model = runtime.prepare(model)?;
        let idt = DatumType::QI8(QParams::ZpScale {
            zero_point: self.qp[2].cast_to_scalar()?,
            scale: *self.qp[3].to_scalar()?,
        });
        let data = self.data.clone().into_tensor().cast_to_dt(idt)?.into_owned().into_tvalue();
        let output = model.run(tvec!(data))?.remove(0);
        eprintln!("reference: {reference:?}\noutput   : {output:?}");
        output.close_enough(&reference, approx)
    }
}

impl Arbitrary for QConvProblem {
    type Parameters = QConvProblemParams;
    type Strategy = BoxedStrategy<QConvProblem>;
    fn arbitrary_with(params: Self::Parameters) -> Self::Strategy {
        let geo_rank = params.conv.geo_rank.clone().unwrap_or(1..4);
        (
            crate::data_format(),
            crate::kernel_format(),
            1usize..=10,
            1usize..=8,
            1usize..=8,
            1usize..=(if params.conv.no_group { 1 } else { 3 }),
            geo_rank.prop_flat_map(crate::shapes),
        )
            .prop_flat_map(
                move |(df, kf, n, mut ci0, mut co0, group, (mut ker_shape, data_shape))| {
                    // FIXME in HWIO order, only regular and depthwise are supported
                    if params.conv.no_arbitrary_grouping && group > 1 {
                        ci0 = 1;
                        co0 = 1;
                    }
                    if kf == KernelFormat::HWIO && group > 1 {
                        ci0 = 1;
                    }
                    let qp = q_params(&params, co0 * group);
                    let shape_in = df.from_n_c_hw(n, ci0 * group, data_shape).unwrap();
                    let data_in = qtensor(shape_in.shape.iter().cloned().collect());
                    match kf {
                        KernelFormat::HWIO => {
                            ker_shape.push(ci0 * group);
                            ker_shape.push(co0)
                        }
                        KernelFormat::OIHW => {
                            ker_shape.insert(0, ci0);
                            ker_shape.insert(0, co0 * group)
                        }
                        KernelFormat::OHWI => {
                            ker_shape.insert(0, co0);
                            ker_shape.push(ci0 * group)
                        }
                    };
                    let kernel = qtensor(ker_shape);
                    let bias = proptest::option::of(
                        qtensor(vec![co0 * group]).prop_map(|a| a.mapv(|v| v as i32)),
                    );
                    (Just((kf, shape_in, co0 * group, group)), data_in, kernel, bias, qp)
                    // FIXME
                },
            )
            .prop_map(|((kernel_format, shape_in, co, group), data, kernel, bias, qp)| {
                QConvProblem { shape_in, co, kernel_format, group, data, kernel, bias, qp }
            })
            .boxed()
    }
}

fn qp_noop_i8() -> [Tensor; 6] {
    [tensor0(0i8), tensor0(1f32), tensor0(0i8), tensor0(1f32), tensor0(0i8), tensor0(1f32)]
}

pub fn suite() -> TractResult<TestSuite> {
    let mut suite = TestSuite::default();

    suite.add_arbitrary::<QConvProblem>("proptest", QConvProblemParams::default());

    suite.add(
        "trivial_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[0i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    suite.add(
        "trivial_1",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[2i8]]).into_dyn(),
            kernel: arr3(&[[[64i8]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    suite.add(
        "trivial_2",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [2]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[-13i8], [26]]).into_dyn(),
            kernel: arr3(&[[[8i8, -2]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    suite.add(
        "trivial_3",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 2, [1]).unwrap(),
            co: 2,
            kernel_format: HWIO,
            group: 1,
            data: arr2(&[[0i8], [0]]).into_dyn(),
            kernel: arr3(&[[[0i8, 0], [0, 0]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    let mut qp = qp_noop_i8();
    qp[1] = tensor1(&[1f32, 0.5]);
    qp[2] = tensor0(-2i8);
    suite.add(
        "weird_4",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1]).unwrap(),
            kernel_format: OIHW,
            co: 2,
            group: 1,
            data: arr2(&[[0i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]], [[7]]]).into_dyn(),
            bias: None,
            qp,
        },
    );

    suite.add(
        "a0_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[1]]).into_dyn(),
            kernel: arr3(&[[[0]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );

    let mut qp = qp_noop_i8();
    qp[0] = tensor0(1i32);
    suite.add(
        "kernel_zp",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1]).unwrap(),
            kernel_format: OIHW,
            co: 1,
            group: 1,
            data: arr2(&[[1i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]]]).into_dyn(),
            bias: None,
            qp,
        },
    );

    let mut qp = qp_noop_i8();
    qp[2] = tensor0(1i32);
    suite.add(
        "b0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            kernel_format: OIHW,
            co: 1,
            group: 1,
            data: arr2(&[[0i8]]).into_dyn(),
            kernel: arr3(&[[[-1i8]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    suite.add(
        "shape_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1, 2]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr3(&[[[0], [0]]]).into_dyn(),
            kernel: arr4(&[[[[0]]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    suite.add(
        "batch_0",
        QConvProblem {
            shape_in: NHWC.from_n_c_hw(3, 1, [2]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr3(&[[[0], [0]], [[0], [0]], [[0], [0]]]).into_dyn(),
            kernel: arr3(&[[[0, 0]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![2, 1, 1]);
    let kernel = arr3(&[[[1]]]).into_dyn();
    suite.add(
        "batch_1",
        QConvProblem {
            shape_in: NHWC.from_n_c_hw(2, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[5] = tensor0(9.274534f32);
    suite.add(
        "scale_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[-1]]).into_dyn(),
            kernel: arr3(&[[[1]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[5] = tensor0(1.1400417f32);
    suite.add(
        "scale_1",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[41]]).into_dyn(),
            kernel: arr3(&[[[1]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[3] = tensor0(0.5f32);
    suite.add(
        "scale_2",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[-1]]).into_dyn(),
            kernel: arr3(&[[[2]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[3] = tensor0(0.5f32);
    qp[5] = tensor0(2f32);
    suite.add(
        "scale_3",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[0i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]]]).into_dyn(),
            bias: Some(arr1(&[35i32]).into_dyn()),
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[4] = tensor0(1i32);
    suite.add(
        "c0_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[0i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    suite.add(
        "group_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 2, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 2,
            data: arr2(&[[0, 0]]).into_dyn(),
            kernel: arr3(&[[[0]], [[0]]]).into_dyn(),
            bias: None,
            qp: qp_noop_i8(),
        },
    );
    let mut qp = qp_noop_i8();
    qp[2] = tensor0(1i32);
    suite.add(
        "group_1",
        QConvProblem {
            shape_in: NCHW.from_n_c_hw(1, 2, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 2,
            data: arr3(&[[[0], [0]]]).into_dyn(),
            kernel: arr3(&[[[1]], [[0]]]).into_dyn(),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[2] = tensor0(1i32);
    suite.add(
        "group_2",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 2, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 2,
            data: arr2(&[[0, 0]]).into_dyn(),
            kernel: arr3(&[[[0]], [[1]]]).into_dyn(),
            bias: None,
            qp,
        },
    );

    let mut qp = qp_noop_i8();
    qp[1] = tensor0(0.5f32);
    qp[2] = tensor0(2i32);
    qp[3] = tensor0(2f32);
    qp[5] = tensor0(2f32);
    suite.add(
        "rounding_0",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[4i8]]).into_dyn(),
            kernel: arr3(&[[[-5]]]).into_dyn(),
            bias: Some(arr1(&[-125i32]).into_dyn()),
            qp,
        },
    );

    let mut qp = qp_noop_i8();
    qp[5] = tensor0(1.3759452f32);
    suite.add(
        "rounding_on_arm",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: arr2(&[[1i8]]).into_dyn(),
            kernel: arr3(&[[[0i8]], [[-15]]]).into_dyn(),
            bias: None,
            qp,
        },
    );

    suite.add(
        "bias_1",
        QConvProblem {
            shape_in: NHWC.from_n_c_hw(1, 1, [1, 1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 1, 1]),
            kernel: ArrayD::zeros(vec![2, 1, 1, 1]),
            bias: Some(tract_ndarray::arr1(&[1, 2]).into_dyn()),
            qp: qp_noop_i8(),
        },
    );

    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![1, 1]);
    let kernel = ArrayD::zeros(vec![2, 1, 1]);
    suite.add(
        "bias_2",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: Some(tract_ndarray::arr1(&[0, 1]).into_dyn()),
            qp,
        },
    );

    let mut qp = qp_noop_i8();
    qp[2] = tensor0(-1);
    let data = ArrayD::zeros(vec![2, 1]);
    let mut kernel = ArrayD::zeros(vec![5, 1, 2]);
    *kernel.as_slice_mut().unwrap().last_mut().unwrap() = -1;
    suite.add(
        "bias_3",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 1, [2]).unwrap(),
            co: 5,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: Some(ArrayD::zeros([5].as_ref())),
            qp,
        },
    );

    suite.add(
        "bias_4",
        QConvProblem {
            shape_in: NHWC.from_n_c_hw(1, 1, [1, 1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 1, 1]),
            kernel: ArrayD::zeros(vec![2, 1, 1, 1]),
            bias: Some(tract_ndarray::arr1(&[0, 1]).into_dyn()),
            qp: qp_noop_i8(),
        },
    );

    suite.add(
        "bias_5",
        QConvProblem {
            shape_in: NHWC.from_n_c_hw(1, 1, [1, 1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 1, 1]),
            kernel: ArrayD::zeros(vec![1, 1, 1, 1]),
            bias: Some(tract_ndarray::arr1(&[1]).into_dyn()),
            qp: qp_noop_i8(),
        },
    );

    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![1, 1]);
    let kernel = ArrayD::zeros(vec![2, 1, 1]);
    suite.add(
        "bias_in_chw",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: Some(ArrayD::zeros([2].as_ref())),
            qp,
        },
    );
    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![1, 1, 1]);
    let kernel = ArrayD::zeros(vec![1, 1, 1]);
    suite.add(
        "bias_with_batch",
        QConvProblem {
            shape_in: NCHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 1,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: Some(arr1(&[1]).into_dyn()),
            qp,
        },
    );
    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![1, 1, 1]);
    let kernel = ArrayD::zeros(vec![2, 1, 1]);
    suite.add(
        "bias_vec_with_batch",
        QConvProblem {
            shape_in: NCHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: Some(arr1(&[0, 1]).into_dyn()),
            qp,
        },
    );
    let qp = qp_noop_i8();
    let data = ArrayD::zeros(vec![1, 2]);
    let kernel = ArrayD::zeros(vec![5, 2, 1]);
    suite.add(
        "asan_0",
        QConvProblem {
            shape_in: HWC.from_n_c_hw(1, 2, [1]).unwrap(),
            co: 5,
            kernel_format: OIHW,
            group: 1,
            data,
            kernel,
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[1] = tensor1(&[1f32, 1f32]);
    suite.add(
        "tflite_per_axis_0",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1]),
            kernel: ArrayD::zeros(vec![2, 1, 1]),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[1] = tensor1(&[1f32, 1f32]);
    suite.add(
        "tflite_per_axis_1",
        QConvProblem {
            shape_in: CHW.from_n_c_hw(1, 1, [1, 2]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 2]),
            kernel: ArrayD::zeros(vec![2, 1, 1, 2]),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[1] = tensor1(&[1f32, 1f32]);
    suite.add(
        "tflite_per_axis_nchw_0",
        QConvProblem {
            shape_in: NCHW.from_n_c_hw(1, 1, [1]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 1]),
            kernel: ArrayD::zeros(vec![2, 1, 1]),
            bias: None,
            qp,
        },
    );
    let mut qp = qp_noop_i8();
    qp[1] = tensor1(&[1f32, 1f32]);
    suite.add(
        "tflite_per_axis_nchw_1",
        QConvProblem {
            shape_in: NCHW.from_n_c_hw(1, 1, [2]).unwrap(),
            co: 2,
            kernel_format: OIHW,
            group: 1,
            data: ArrayD::zeros(vec![1, 1, 2]),
            kernel: ArrayD::zeros(vec![2, 1, 2]),
            bias: None,
            qp,
        },
    );
    Ok(suite)
}
