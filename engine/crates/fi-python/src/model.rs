//! Models: inputs read point in time (`fi.Input`), and built-in or Python models (`fi.Model`).

use super::*;

/// A model input: `series.column`, read exactly at each time, as of it, market-wide, or through
/// a relationship.
#[pyclass(name = "Input", module = "fi_engine._native", frozen)]
#[derive(Clone)]
pub struct PyInput {
    pub(crate) input: Input,
}

#[pymethods]
impl PyInput {
    /// The value at exactly the evaluation time.
    #[staticmethod]
    fn exact(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::exact(s, c) })
    }

    /// The latest value at or before the evaluation time (static attributes, filings).
    #[staticmethod]
    fn asof(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::asof(s, c) })
    }

    /// A market-wide value (a series with no entity), as of the evaluation time.
    #[staticmethod]
    fn market(spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: Input::market(s, c) })
    }

    /// The same input, reached through relationship `series.column` (a bond's issuer).
    fn via(&self, spec: &str) -> PyResult<Self> {
        let (s, c) = split(spec)?;
        Ok(Self { input: self.input.clone().via(s, c) })
    }

    fn __repr__(&self) -> String {
        let i = &self.input;
        let how = if i.market {
            "market"
        } else if i.asof {
            "asof"
        } else {
            "exact"
        };
        let via = i.via.as_ref().map_or(String::new(), |(s, c)| format!(".via({s}.{c})"));
        format!("Input.{how}({}.{}){via}", i.series, i.column)
    }
}

/// An input from an `Input`, or from `"series.column"` read exactly (or as of, if `asof`).
pub(crate) fn input(obj: &Bound<'_, PyAny>, asof: bool) -> PyResult<Input> {
    if let Ok(i) = obj.cast::<PyInput>() {
        return Ok(i.get().input.clone());
    }
    let spec: String = obj.extract().map_err(|_| PyTypeError::new_err("an input is an Input or a \"series.column\" string"))?;
    let (s, c) = split(&spec)?;
    Ok(if asof { Input::asof(s, c) } else { Input::exact(s, c) })
}

/// A model: built in, or a Python callable. Evaluating it writes a series named after it.
#[pyclass(name = "Model", module = "fi_engine._native", frozen)]
pub struct PyModel {
    pub(crate) model: fi_kernel::Model,
    pub(crate) python: Option<Arc<PyCrossSection>>,
    /// Where the Python callables inside it (a deal manager's decisions) keep their exceptions.
    pub(crate) errors: Vec<Arc<Errors>>,
}

#[pymethods]
impl PyModel {
    /// Z-score of `input` across the universe at each time, within groups if `group` is given
    /// (a category, read as of the time).
    #[staticmethod]
    #[pyo3(signature = (name, input, *, group = None, ddof = 0.0, min_count = 2))]
    fn zscore(name: &str, input: &Bound<'_, PyAny>, group: Option<&Bound<'_, PyAny>>, ddof: f64, min_count: usize) -> PyResult<Self> {
        let m = ZScore { name: name.into(), input: self::input(input, false)?, group: group.map(|g| self::input(g, true)).transpose()?, ddof, min_count };
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// A Nelson–Siegel curve fitted each date to `yields` against `maturity` (a date column, in
    /// days since the epoch): outputs `fitted` and `residual`.
    #[staticmethod]
    #[pyo3(signature = (name, yields, maturity, *, lam = 1.8, min_points = 5))]
    fn nelson_siegel(name: &str, yields: &Bound<'_, PyAny>, maturity: &Bound<'_, PyAny>, lam: f64, min_points: usize) -> PyResult<Self> {
        let m = NelsonSiegel { name: name.into(), yields: input(yields, false)?, maturity: input(maturity, true)?, lambda: lam, min_points };
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// Exponentially weighted volatility of log changes in a level: output `vol`.
    #[staticmethod]
    #[pyo3(signature = (name, input, *, halflife = 20.0, annualise = 252.0, warmup = 5))]
    fn ewma_vol(name: &str, input: &Bound<'_, PyAny>, halflife: f64, annualise: f64, warmup: usize) -> PyResult<Self> {
        let m = EwmaVol { name: name.into(), input: self::input(input, false)?, halflife, annualise, warmup };
        Ok(Self { model: fi_kernel::Model::time_series(m), python: None, errors: Vec::new() })
    }

    /// Merton's model per entity: equity (exact), debt (as of: filings) and a rate (market-wide):
    /// outputs `dd`, `pd` and `spread`.
    #[staticmethod]
    #[pyo3(signature = (name, equity, debt, rate, *, horizon = 1.0, window = 252, annualise = 252.0))]
    fn merton(
        name: &str,
        equity: &Bound<'_, PyAny>,
        debt: &Bound<'_, PyAny>,
        rate: &Bound<'_, PyAny>,
        horizon: f64,
        window: usize,
        annualise: f64,
    ) -> PyResult<Self> {
        let rate = match rate.extract::<String>() {
            Ok(spec) => {
                let (s, c) = split(&spec)?;
                Input::market(s, c)
            }
            Err(_) => input(rate, false)?,
        };
        let m = Merton { name: name.into(), equity: input(equity, false)?, debt: input(debt, true)?, rate, horizon, window, annualise };
        Ok(Self { model: fi_kernel::Model::time_series(m), python: None, errors: Vec::new() })
    }

    /// A Python cross-sectional model: `compute(t, entities, x)` is called once per time, in time
    /// order, with `x` one NumPy array per input over the universe; it returns one array per
    /// output (or a dict by output name). Each Python model is its own cache entry unless a
    /// `version` says two are the same.
    #[staticmethod]
    #[pyo3(signature = (name, compute, inputs, outputs, *, universe = None, window_days = 1.0, lag_days = 0.0, version = None))]
    #[allow(clippy::too_many_arguments)]
    fn python(
        name: &str,
        compute: Py<PyAny>,
        inputs: Vec<Bound<'_, PyAny>>,
        outputs: Vec<String>,
        universe: Option<String>,
        window_days: f64,
        lag_days: f64,
        version: Option<u64>,
    ) -> PyResult<Self> {
        let params = version.map_or_else(bridge::fresh, |v| {
            let mut h = Hash64::new();
            h.str("python");
            h.u64(v);
            h.finish()
        });
        let mut spec = ModelSpec::new(name, Shape::CrossSection).serial().window(days(window_days)).lag(days(lag_days)).params(params);
        for i in &inputs {
            spec = spec.input(input(i, false)?);
        }
        for o in &outputs {
            spec = spec.output(o);
        }
        if let Some(u) = &universe {
            spec = spec.universe(u);
        }
        let p = Arc::new(PyCrossSection::new(spec, compute));
        Ok(Self { model: fi_kernel::Model::CrossSection(p.clone()), python: Some(p), errors: Vec::new() })
    }

    /// The model's name (its output series).
    #[getter]
    fn name(&self) -> String {
        self.model.spec().name
    }

    /// The same model with a role: `"signal"` (the default), `"real_world"` (how the world
    /// evolves), `"pricing"` (values from a state) or `"stress"` (a named scenario override).
    fn role(&self, role: &str) -> PyResult<Self> {
        let r = parse_role(role)?;
        Ok(self.amended(move |s| s.role(r)))
    }

    /// The same model, available only from `t`: a point-in-time run has no output before then.
    fn available_from(&self, t: &Bound<'_, PyAny>) -> PyResult<Self> {
        let t = convert::ts(t)?;
        Ok(self.amended(move |s| s.available_from(t)))
    }

    /// The same model used before it was available: a labelled retrospective experiment.
    fn retrospective(&self) -> Self {
        self.amended(ModelSpec::retrospective)
    }

    /// The same model, declaring the state its output describes.
    fn describes(&self, state: String) -> Self {
        self.amended(move |s| s.describes(&state))
    }

    /// Versions of one cross-sectional model, each used from when it became available:
    /// `versions` is a list of `(time, model)`. A retrospective run uses the latest throughout.
    #[staticmethod]
    #[pyo3(signature = (name, versions, *, retrospective = false))]
    fn vintaged(name: &str, versions: Vec<(Bound<'_, PyAny>, PyRef<'_, PyModel>)>, retrospective: bool) -> PyResult<Self> {
        let v = versions.iter().map(|(t, m)| Ok((convert::ts(t)?, m.model.clone()))).collect::<PyResult<Vec<_>>>()?;
        let m = Vintaged::new(name, v, retrospective).map_err(to_py)?;
        Ok(Self { model: fi_kernel::Model::cross_section(m), python: None, errors: Vec::new() })
    }

    /// The series it writes.
    #[getter]
    fn writes(&self) -> Vec<String> {
        self.model.spec().produced()
    }

    /// What the model's definition models (a deal's waterfall features, a generator's).
    #[getter]
    fn supports(&self) -> Vec<&'static str> {
        match &self.model {
            fi_kernel::Model::Batch(b) => b.supports(),
            _ => Vec::new(),
        }
    }

    /// Output columns.
    #[getter]
    fn outputs(&self) -> Vec<String> {
        self.model.spec().outputs
    }

    fn __repr__(&self) -> String {
        let s = self.model.spec();
        let kind = if self.python.is_some() { "python" } else { "built-in" };
        format!("Model({}, {kind}, {:?}, outputs={:?})", s.name, s.shape, s.outputs)
    }
}

impl PyModel {
    pub(crate) fn amended(&self, f: impl Fn(ModelSpec) -> ModelSpec + Send + Sync + 'static) -> Self {
        Self { model: self.model.clone().amend(f), python: self.python.clone(), errors: self.errors.clone() }
    }
}

pub(crate) fn parse_role(role: &str) -> PyResult<ModelRole> {
    match role {
        "signal" => Ok(ModelRole::Signal),
        "real_world" => Ok(ModelRole::RealWorld),
        "pricing" => Ok(ModelRole::Pricing),
        "stress" => Ok(ModelRole::Stress),
        r => Err(PyValueError::new_err(format!("a role is signal, real_world, pricing or stress, not {r}"))),
    }
}

pub(crate) fn role_name(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Signal => "signal",
        ModelRole::RealWorld => "real_world",
        ModelRole::Pricing => "pricing",
        ModelRole::Stress => "stress",
    }
}
