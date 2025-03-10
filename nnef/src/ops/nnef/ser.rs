use crate::ast::QuantFormat;
use crate::internal::*;
use crate::ser::*;
use tract_core::num_traits::Zero;
use tract_core::ops;
use tract_core::ops::cnn::PoolSpec;
use tract_core::ops::matmul::MatMulAxes;
use tract_core::ops::nn::DataFormat;

pub fn source(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::source::TypedSource,
) -> TractResult<Option<Arc<RValue>>> {
    if let Some(shape) = op.fact.shape.as_concrete() {
        if op.fact.datum_type == DatumType::F32 {
            return Ok(Some(invocation("external", &[], &[("shape", ints(shape))])));
        } else if op.fact.datum_type.is_quantized() {
            if let Some(qp) = QuantFormat::from_dt(node.outputs[0].fact.datum_type) {
                ast.quantization.insert(node.name.clone(), qp);
            }
            return Ok(Some(invocation("external", &[], &[("shape", ints(shape))])));
        }
    };
    Ok(None)
}

pub fn konst(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::konst::Const,
) -> TractResult<Option<Arc<RValue>>> {
    Ok(Some(ast.konst(&node.name, &op.0)?))
}

pub fn concat(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::array::TypedConcat,
) -> TractResult<Option<Arc<RValue>>> {
    let wires = node
        .inputs
        .iter()
        .map(|i| Ok(ast.mapping[i].as_ref().clone()))
        .collect::<TractResult<TVec<RValue>>>()?;
    Ok(Some(invocation("concat", &[array(&wires).into()], &[("axis", numeric(op.axis))])))
}

pub fn slice(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::array::Slice,
) -> TractResult<Option<Arc<RValue>>> {
    let wire = ast.mapping[&node.inputs[0]].clone();
    // end = 0 means "to the end" in early nnef specs.
    // the case begin = 0, end = 0: tract says "empty tensor", but nnef says "noop"
    // so serialize as begin = 0, end = -dim
    let end = if op.end.is_zero() && op.start == op.end {
        -ast.model.node_input_facts(node.id)?[0].shape[op.axis].clone()
    } else {
        op.end.clone()
    };
    dbg!(Ok(Some(invocation(
        "slice",
        &[wire],
        &[
            ("axes", ints(&[op.axis])),
            ("begin", tdims(&[op.start.clone()])),
            ("end", tdims(&[end]))
        ],
    ))))
}

pub fn tile(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::array::Tile,
) -> TractResult<Option<Arc<RValue>>> {
    let wire = ast.mapping[&node.inputs[0]].clone();
    Ok(Some(invocation("tile", &[wire], &[("repeats", tdims(&op.multipliers))])))
}

pub fn pad(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::array::Pad,
) -> TractResult<Option<Arc<RValue>>> {
    use ops::array::PadMode;
    let wire = ast.mapping[&node.inputs[0]].clone();
    let dt = ast.model.outlet_fact(node.inputs[0])?.datum_type;
    let padding = array(&op.pads.iter().map(|pair| ints(&[pair.0, pair.1])).collect::<TVec<_>>());
    let mut params = tvec!(("padding", padding));
    let border = match &op.mode {
        PadMode::Constant(c) => {
            params.push((
                "value",
                if dt.is_float() {
                    numeric(c.cast_to_scalar::<f32>()?)
                } else {
                    numeric(c.cast_to_scalar::<i64>()?)
                },
            ));
            "constant"
        }
        PadMode::Reflect => "reflect",
        PadMode::Edge => "replicated",
    };
    params.push(("border", string(border)));
    Ok(Some(invocation("pad", &[wire], &params)))
}

fn data_into_ncwh(data_format: DataFormat, geo_rank: usize, mut wire: Arc<RValue>) -> Arc<RValue> {
    use tract_core::ops::nn::DataFormat::*;
    if !data_format.has_n() {
        wire = invocation("unsqueeze", &[wire], &[("axes", ints(&[0]))]);
    }
    if data_format == NHWC || data_format == HWC {
        let mut perm: TVec<usize> = (0..geo_rank + 2).collect();
        perm[1..].rotate_right(1);
        wire = invocation("transpose", &[wire], &[("axes", ints(&perm))])
    }
    wire
}

fn data_from_ncwh(data_format: DataFormat, geo_rank: usize, mut wire: Arc<RValue>) -> Arc<RValue> {
    use tract_core::ops::nn::DataFormat::*;
    if data_format == NHWC || data_format == HWC {
        let mut perm: TVec<usize> = (0..geo_rank + 2).collect();
        perm[1..].rotate_left(1);
        wire = invocation("transpose", &[wire], &[("axes", ints(&perm))])
    }
    if !data_format.has_n() {
        wire = invocation("squeeze", &[wire], &[("axes", ints(&[0]))]);
    }
    wire
}

pub fn make_conv_named_args<'a>(
    node: &'a TypedNode,
    pool_spec: &'a PoolSpec,
    group: usize,
    deconv: bool,
    adjustments: Option<&[usize]>,
) -> TractResult<TVec<(&'a str, RValue)>> {
    use tract_core::ops::cnn::PaddingSpec;
    let output_shape = pool_spec.data_format.shape(node.outputs[0].fact.shape.to_tvec())?;
    let padding = match &pool_spec.padding {
        PaddingSpec::Explicit(bef, after, _) => array(
            &bef.iter()
                .zip(after.iter())
                .map(|(a, b)| tuple_2(numeric(a), numeric(b)))
                .collect::<Vec<_>>(),
        ),
        PaddingSpec::SameUpper => array(&[]),
        PaddingSpec::SameLower => bail!("Unsupported padding scheme"),
        PaddingSpec::Valid => array(
            (0..pool_spec.rank()).map(|_| tuple_2(numeric(0), numeric(0))).collect::<Vec<_>>(),
        ),
    };
    let mut named_args = tvec![
        ("dilation", ints(&pool_spec.dilations())),
        ("stride", ints(&pool_spec.strides())),
        ("border", string("constant")),
        ("groups", numeric(group)),
        ("padding", padding),
    ];
    if deconv && adjustments.unwrap().iter().any(|a| *a != 0) {
        let output_shape = output_shape
            .hw_dims()
            .iter()
            .map(|d| d.to_usize())
            .collect::<TractResult<TVec<_>>>()?;
        named_args.push(("output_shape", ints(&output_shape)));
    };
    Ok(named_args)
}

#[allow(clippy::too_many_arguments)]
pub fn conv_or_deconv(
    ast: &mut IntoAst,
    node: &TypedNode,
    pool_spec: &PoolSpec,
    weights: Tensor,
    bias: &Option<Arc<Tensor>>,
    group: usize,
    deconv: bool,
    adjustments: Option<&[usize]>,
) -> TractResult<Option<Arc<RValue>>> {
    let mut wire = ast.mapping[&node.inputs[0]].clone();
    let data_format = pool_spec.data_format;
    if !data_format.has_n() {
        wire = invocation("unsqueeze", &[wire], &[("axes", ints(&[0]))]);
    }
    if data_format.c_is_last() {
        let mut perm: TVec<usize> = (0..pool_spec.rank() + 1).collect();
        perm.insert(1, pool_spec.rank() + 1);
        wire = invocation("transpose", &[wire], &[("axes", ints(&perm))]);
    }
    wire = ast.force_variable(format!("{}_input", node.name), &wire);

    let mut inputs = tvec![wire];
    inputs.push(ast.konst_variable(format!("{}_weigths", node.name), &weights.into_arc_tensor())?);
    if let Some(bias) = bias.as_ref() {
        inputs.push(ast.konst(format!("{}_bias", node.name), bias)?);
    }

    let named_args = make_conv_named_args(node, pool_spec, group, deconv, adjustments)?;

    let name = if deconv { "deconv" } else { "conv" };
    wire = invocation(name, &inputs, &named_args);
    // need to force quantization storage as output code may miss it
    let var_name = format!("{}_{}", node.name, name);
    if let Some(qp) = QuantFormat::from_dt(node.outputs[0].fact.datum_type) {
        ast.quantization.insert(var_name.clone(), qp);
    }
    wire = ast.force_variable(var_name, &wire);

    if data_format.c_is_last() {
        let mut perm: TVec<usize> = (0..pool_spec.rank() + 2).collect();
        perm.remove(1);
        perm.push(1);
        wire = invocation("transpose", &[wire], &[("axes", ints(&perm))]);
    }
    if !data_format.has_n() {
        wire = invocation("squeeze", &[wire], &[("axes", ints(&[0]))]);
    }

    Ok(Some(wire))
}

pub fn conv(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::cnn::conv::ConvUnary,
) -> TractResult<Option<Arc<RValue>>> {
    if op.q_params.is_some() && !node.outputs[0].fact.datum_type.is_quantized() {
        return Ok(None);
    }
    // tract HWIO: H W I/g O
    // tract OIHW: O I/g H W
    // nnef: O I/g H W
    let mut kernel = op.kernel.clone().into_tensor();
    if op.kernel_fmt == ops::cnn::KernelFormat::HWIO {
        let geo_rank = op.kernel.rank() - 2;
        kernel = kernel.move_axis(geo_rank, 0)?.move_axis(geo_rank + 1, 0)?;
    }
    conv_or_deconv(ast, node, &op.pool_spec, kernel, &op.bias, op.group, false, None)
}

pub fn deconv(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::cnn::deconv::DeconvUnary,
) -> TractResult<Option<Arc<RValue>>> {
    // tract HWIO: H W I O/g -> tract OIHW: O/g I H W
    let mut kernel = op.kernel.clone().into_tensor();
    if op.kernel_format == ops::cnn::KernelFormat::HWIO {
        let geo_rank = op.kernel.rank() - 2;
        kernel = kernel.move_axis(geo_rank, 0)?.move_axis(geo_rank + 1, 0)?;
    }
    // tract: O/g I H W -> O I/g H W
    kernel = kernel.split_axis(1, op.group)?.move_axis(1, 0)?.collapse_axis_with_next(0);
    conv_or_deconv(
        ast,
        node,
        &op.pool_spec,
        kernel,
        &op.bias,
        op.group,
        true,
        Some(&op.adjustments),
    )
}

fn cnn_pool_fragment<'a>(
    ast: &'a mut IntoAst,
    data_format: DataFormat,
    geo_rank: usize,
    op_name: &str,
) -> String {
    if data_format == DataFormat::NCHW {
        return op_name.into();
    }
    let fragment_name = format!("tract_sum_pool_{:?}_{}D", data_format, geo_rank).to_lowercase();
    if ast.fragments.contains_key(&fragment_name) {
        return fragment_name;
    }

    let mut body = vec![];
    let mut fragment = ast.framework.stdlib.iter().find(|f| f.decl.id == op_name).unwrap().clone();
    fragment.decl.id = fragment_name.clone();

    let mut wire = ident("input").into();
    wire = data_into_ncwh(data_format, geo_rank, wire);

    body.push(assignment("nchw", wire));
    wire = invocation(
        op_name,
        &[ident("nchw").into()],
        &fragment
            .decl
            .parameters
            .iter()
            .skip(1)
            .map(|f| (&*f.id, ident(&f.id)))
            .collect::<Vec<_>>(),
    );
    body.push(assignment("sum_pool", wire));

    wire = data_from_ncwh(data_format, geo_rank, ident("sum_pool").into());

    body.push(assignment("output", wire));
    fragment.body = Some(body);
    ast.fragments.insert(fragment_name.clone(), fragment);
    fragment_name
}

fn cnn_pool(
    ast: &mut IntoAst,
    node: &TypedNode,
    op_name: &str,
    pool_spec: &tract_core::ops::cnn::PoolSpec,
    normalize_arg: Option<(&'static str, RValue)>,
) -> TractResult<Option<Arc<RValue>>> {
    use tract_core::ops::cnn::PaddingSpec;
    let mut wire = ast.mapping[&node.inputs[0]].clone();
    wire = ast.force_variable(format!("{}_input", node.name), &wire);
    let conv_fragment = cnn_pool_fragment(ast, pool_spec.data_format, pool_spec.rank(), op_name);
    let padding = match &pool_spec.padding {
        PaddingSpec::Explicit(bef, after, _) => Some(
            bef.iter()
                .zip(after.iter())
                .map(|(a, b)| tuple_2(numeric(a), numeric(b)))
                .collect::<Vec<_>>(),
        ),
        PaddingSpec::SameUpper => None,
        PaddingSpec::SameLower => bail!("Unsupported padding scheme"),
        PaddingSpec::Valid => {
            Some((0..pool_spec.rank()).map(|_| tuple_2(numeric(0), numeric(0))).collect::<Vec<_>>())
        }
    };
    let mut size = tvec!(1, 1);
    size.extend(pool_spec.kernel_shape.iter().cloned());
    let mut strides = tvec!(1, 1);
    strides.extend(pool_spec.strides().iter().cloned());
    let mut dilations = tvec!(1, 1);
    dilations.extend(pool_spec.dilations().iter().cloned());
    let padding = if let Some(pad) = padding {
        let mut full_padding =
            vec![tuple_2(numeric(0), numeric(0)), tuple_2(numeric(0), numeric(0))];
        full_padding.extend(pad.iter().cloned());
        array(full_padding)
    } else {
        array(&[])
    };
    let mut params = tvec!(
        ("size", ints(&size)),
        ("dilation", ints(&dilations)),
        ("stride", ints(&strides)),
        ("border", string("ignore")),
        ("padding", padding),
    );
    if let Some(normalize_arg) = normalize_arg {
        params.push(normalize_arg);
    };
    wire = invocation(&conv_fragment, &[wire], &params);
    wire = ast.force_variable(&node.name, &wire);
    Ok(Some(wire))
}

pub fn max_pool(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::cnn::MaxPool,
) -> TractResult<Option<Arc<RValue>>> {
    cnn_pool(ast, node, "max_pool", &op.pool_spec, None)
}

pub fn sum_pool(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::cnn::SumPool,
) -> TractResult<Option<Arc<RValue>>> {
    cnn_pool(ast, node, "box", &op.pool_spec, Some(("normalize", logical(op.normalize))))
}

pub fn axis_op(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::change_axes::AxisOp,
) -> TractResult<Option<Arc<RValue>>> {
    let wire = ast.mapping[&node.inputs[0]].clone();
    let invoke = match op {
        AxisOp::Rm(axis) => invocation("squeeze", &[wire], &[("axes", ints(&[*axis]))]),
        AxisOp::Add(axis) => invocation("unsqueeze", &[wire], &[("axes", ints(&[*axis]))]),
        AxisOp::Move(from, to) => {
            let rank = node.outputs[0].fact.rank();
            let mut perm: TVec<usize> = (0..rank).collect();
            if from < to {
                perm[*from..(to + 1)].rotate_left(1);
            } else {
                perm[*to..(from + 1)].rotate_right(1);
            }
            invocation("transpose", &[wire], &[("axes", ints(&perm))])
        }
        AxisOp::Reshape(start, from, to) => invocation(
            "reshape",
            &[wire],
            &[
                ("shape", tdims(to)),
                ("axis_start", numeric(start)),
                ("axis_count", numeric(from.len())),
            ],
        ),
    };
    Ok(Some(invoke))
}

pub fn reduce(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::nn::Reduce,
) -> TractResult<Option<Arc<RValue>>> {
    let wire = ast.mapping[&node.inputs[0]].clone();
    let oper = match op.reducer {
        ops::nn::Reducer::ArgMax(last) if !last => "argmax_reduce",
        ops::nn::Reducer::ArgMin(last) if !last => "argmin_reduce",
        ops::nn::Reducer::Sum => "sum_reduce",
        ops::nn::Reducer::Max => "max_reduce",
        ops::nn::Reducer::Min => "min_reduce",
        _ => return Ok(None),
    };
    Ok(Some(invocation(oper, &[wire], &[("axes", ints(&op.axes))])))
}

pub fn matmulaxes_as_transpositions(
    axes: &MatMulAxes,
    a_rank: usize,
    b_rank: usize,
) -> Option<(bool, bool, bool)> {
    fn check_one(axis_1: usize, axis_2: usize, rank: usize) -> bool {
        (axis_1 as isize - axis_2 as isize).abs() == 1 && axis_1.max(axis_2) == rank - 1
    }
    if check_one(axes.a_k, axes.a_m, a_rank)
        && check_one(axes.b_k, axes.b_n, b_rank)
        && check_one(axes.c_m, axes.c_n, a_rank.max(b_rank))
    {
        Some((axes.a_m > axes.a_k, axes.b_k > axes.b_n, axes.c_m > axes.c_n))
    } else {
        None
    }
}

pub fn matmul(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::matmul::MatMul,
) -> TractResult<Option<Arc<RValue>>> {
    let a = ast.force_variable(format!("{}_a", node.name), &ast.mapping[&node.inputs[0]].clone());
    let b = ast.force_variable(format!("{}_b", node.name), &ast.mapping[&node.inputs[1]].clone());
    let inputs = ast.model.node_input_facts(node.id)?;
    let c = if let Some((a_trans, b_trans, c_trans)) =
        matmulaxes_as_transpositions(&op.axes, inputs[0].rank(), inputs[1].rank())
    {
        if c_trans {
            invocation(
                "matmul",
                &[b, a],
                &[("transposeA", logical(!b_trans)), ("transposeB", logical(!a_trans))],
            )
        } else {
            invocation(
                "matmul",
                &[a, b],
                &[("transposeA", logical(a_trans)), ("transposeB", logical(b_trans))],
            )
        }
    } else {
        return Ok(None);
    };
    Ok(Some(ast.force_variable(&node.name, &c)))
}

pub fn qmatmul(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::matmul::QMatMul,
) -> TractResult<Option<Arc<RValue>>> {
    if !node.outputs[0].fact.datum_type.is_quantized() {
        return Ok(None);
    }
    let a = ast.force_variable(format!("{}_a", node.name), &ast.mapping[&node.inputs[0]].clone());
    let b = ast.force_variable(format!("{}_b", node.name), &ast.mapping[&node.inputs[1]].clone());
    let inputs = ast.model.node_input_facts(node.id)?;
    let c = if let Some((a_trans, b_trans, c_trans)) =
        matmulaxes_as_transpositions(&op.axes, inputs[0].rank(), inputs[1].rank())
    {
        if c_trans {
            invocation(
                "matmul",
                &[b, a],
                &[("transposeA", logical(!b_trans)), ("transposeB", logical(!a_trans))],
            )
        } else {
            invocation(
                "matmul",
                &[a, b],
                &[("transposeA", logical(a_trans)), ("transposeB", logical(b_trans))],
            )
        }
    } else {
        return Ok(None);
    };
    Ok(Some(ast.force_variable(&node.name, &c)))
}

pub fn matmul_unary(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::matmul::MatMulUnary,
) -> TractResult<Option<Arc<RValue>>> {
    let a = ast.konst(format!("{}_a", node.name), &op.a)?;
    let b = ast.force_variable(format!("{}_b", node.name), &ast.mapping[&node.inputs[0]].clone());
    let b_input = ast.model.outlet_fact(node.inputs[0])?;
    let c = if let Some((a_trans, b_trans, c_trans)) =
        matmulaxes_as_transpositions(&op.axes, op.a.rank(), b_input.rank())
    {
        if c_trans {
            invocation(
                "matmul",
                &[b, a],
                &[("transposeA", logical(!b_trans)), ("transposeB", logical(!a_trans))],
            )
        } else {
            invocation(
                "matmul",
                &[a, b],
                &[("transposeA", logical(a_trans)), ("transposeB", logical(b_trans))],
            )
        }
    } else {
        return Ok(None);
    };
    Ok(Some(ast.force_variable(&node.name, &c)))
}

pub fn select(
    ast: &mut IntoAst,
    node: &TypedNode,
    _op: &ops::logic::Iff,
) -> TractResult<Option<Arc<RValue>>> {
    Ok(Some(invocation(
        "select",
        &node.inputs.iter().map(|o| ast.mapping[o].clone()).collect::<TVec<_>>(),
        &[],
    )))
}

pub fn leaky_relu(ast: &mut IntoAst, node: &TypedNode) -> TractResult<Option<Arc<RValue>>> {
    let op = node.op_as::<ops::element_wise::ElementWiseOp>().context("Wrong op")?;
    let op = op.0.downcast_ref::<ops::nn::LeakyRelu>().context("Wrong op")?;
    Ok(Some(invocation(
        "leaky_relu",
        &node.inputs.iter().map(|o| ast.mapping[o].clone()).collect::<TVec<_>>(),
        &[("alpha", RValue::Literal(op.alpha.into()))],
    )))
}

pub fn softmax(
    ast: &mut IntoAst,
    node: &TypedNode,
    op: &ops::nn::Softmax,
) -> TractResult<Option<Arc<RValue>>> {
    let litteral_axes: Vec<_> = op.axes.iter().map(|&it| (it as i64).into()).collect();
    Ok(Some(invocation(
        "softmax",
        &[ast.mapping[&node.inputs[0]].clone()],
        &[("axes", RValue::Literal(crate::ast::Literal::Array(litteral_axes)))],
    )))
}
