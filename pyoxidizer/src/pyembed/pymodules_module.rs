// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// This module defines the _pymodules Python module, which exposes
/// .py/.pyc source/code data so it can be used by an in-memory importer.
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use byteorder::{LittleEndian, ReadBytesExt};
use cpython::exc::{KeyError, ValueError};
use cpython::{
    py_class, py_class_impl, py_coerce_item, PyBool, PyErr, PyModule, PyObject, PyResult, PyString,
    Python, PythonObject, ToPyObject,
};
use python3_sys as pyffi;
use python3_sys::{PyBUF_READ, PyMemoryView_FromMemory};

use super::data::{PYC_MODULES_DATA, PY_MODULES_DATA};
use super::pyinterp::PYMODULES_NAME;

/// Parse modules blob data into a map of module name to module data.
fn parse_modules_blob(data: &'static [u8]) -> Result<HashMap<&str, &[u8]>, &'static str> {
    if data.len() < 4 {
        return Err("modules data too small");
    }

    let mut reader = Cursor::new(data);

    let count = reader.read_u32::<LittleEndian>().unwrap();
    let mut index = Vec::with_capacity(count as usize);
    let mut total_names_length = 0;

    let mut i = 0;
    while i < count {
        let name_length = reader.read_u32::<LittleEndian>().unwrap() as usize;
        let data_length = reader.read_u32::<LittleEndian>().unwrap() as usize;

        index.push((name_length, data_length));
        total_names_length = total_names_length + name_length;
        i = i + 1;
    }

    let mut res = HashMap::with_capacity(count as usize);
    let values_start_offset = reader.position() as usize + total_names_length;
    let mut values_current_offset: usize = 0;

    for (name_length, value_length) in index {
        let offset = reader.position() as usize;

        let name = unsafe { std::str::from_utf8_unchecked(&data[offset..offset + name_length]) };

        let value_offset = values_start_offset + values_current_offset;
        let value = &data[value_offset..value_offset + value_length];
        reader.set_position(offset as u64 + name_length as u64);
        values_current_offset = values_current_offset + value_length;

        res.insert(name, value);
    }

    Ok(res)
}

#[allow(unused_doc_comments)]
/// Python type to facilitate access to in-memory modules data.
///
/// We /could/ use simple Python data structures (e.g. dict mapping
/// module names to data). However, if we pre-populated a Python dict,
/// we'd need to allocate PyObject instances for every value. This adds
/// overhead to startup. This type minimizes PyObject instantiation to
/// reduce overhead.
py_class!(class ModulesType |py| {
    data py_modules: HashMap<&'static str, &'static [u8]>;
    data pyc_modules: HashMap<&'static str, &'static [u8]>;
    data packages: HashSet<&'static str>;

    def get_source(&self, name: PyString) -> PyResult<PyObject> {
        let key = name.to_string(py)?;

        return match self.py_modules(py).get(&*key) {
            Some(value) => {
                let py_value = unsafe {
                    let ptr = PyMemoryView_FromMemory(value.as_ptr() as * mut i8, value.len() as isize, PyBUF_READ);
                    PyObject::from_owned_ptr_opt(py, ptr)
                }.unwrap();

                Ok(py_value)
            },
            None => Err(PyErr::new::<KeyError, _>(py, "module not available"))
        }
    }

    def get_code(&self, name: PyString) -> PyResult<PyObject> {
        let key = name.to_string(py)?;

        return match self.pyc_modules(py).get(&*key) {
            Some(value) => {
                let py_value = unsafe {
                    let ptr = PyMemoryView_FromMemory(value.as_ptr() as * mut i8, value.len() as isize, PyBUF_READ);
                    PyObject::from_owned_ptr_opt(py, ptr)
                }.unwrap();

                Ok(py_value)
            },
            None => Err(PyErr::new::<KeyError, _>(py, "module not available"))
        }
    }

    def has_module(&self, name: PyString) -> PyResult<PyBool> {
        let key = name.to_string(py)?;

        if self.py_modules(py).contains_key(&*key) {
            return Ok(true.to_py_object(py));
        }

        if self.pyc_modules(py).contains_key(&*key) {
            return Ok(true.to_py_object(py));
        }

        return Ok(false.to_py_object(py));
    }

    def is_package(&self, name: PyString) -> PyResult<PyBool> {
        let key = name.to_string(py)?;

        Ok(match self.packages(py).contains(&*key) {
            true => true.to_py_object(py),
            false => false.to_py_object(py),
        })
    }
});

fn populate_packages(packages: &mut HashSet<&'static str>, name: &'static str) {
    let mut search = name;

    loop {
        match search.rfind(".") {
            Some(idx) => {
                packages.insert(&search[0..idx]);
                search = &search[0..idx];
            }
            None => break,
        };
    }
}

/// Construct the global ModulesType instance from an embedded data structure.
fn make_modules(py: Python) -> PyResult<ModulesType> {
    let py_modules = match parse_modules_blob(PY_MODULES_DATA) {
        Ok(value) => value,
        Err(msg) => return Err(PyErr::new::<ValueError, _>(py, msg)),
    };

    let pyc_modules = match parse_modules_blob(PYC_MODULES_DATA) {
        Ok(value) => value,
        Err(msg) => return Err(PyErr::new::<ValueError, _>(py, msg)),
    };

    // TODO consider baking set of packages into embedded data.
    let mut packages: HashSet<&'static str> = HashSet::with_capacity(pyc_modules.len());

    for key in py_modules.keys() {
        populate_packages(&mut packages, key);
    }

    for key in pyc_modules.keys() {
        populate_packages(&mut packages, key);
    }

    ModulesType::create_instance(py, py_modules, pyc_modules, packages)
}

const DOC: &'static [u8] = b"Binary representation of Python modules\0";

static mut MODULE_DEF: pyffi::PyModuleDef = pyffi::PyModuleDef {
    m_base: pyffi::PyModuleDef_HEAD_INIT,
    m_name: PYMODULES_NAME.as_ptr() as *const _,
    m_doc: DOC.as_ptr() as *const _,
    m_size: 0,
    m_methods: 0 as *mut _,
    m_slots: 0 as *mut _,
    m_traverse: None,
    m_clear: None,
    m_free: None,
};

fn init(py: Python, m: &PyModule) -> PyResult<()> {
    let modules = make_modules(py)?;
    m.add(py, "MODULES", modules)?;

    Ok(())
}

/// Module initialization function.
///
/// This creates the Python module object.
///
/// We don't use the macros in the cpython crate because they are somewhat
/// opinionated about how things should work. e.g. they call
/// PyEval_InitThreads(), which is undesired. We want total control.
#[allow(non_snake_case)]
pub unsafe extern "C" fn PyInit__pymodules() -> *mut pyffi::PyObject {
    let py = cpython::Python::assume_gil_acquired();
    let module = pyffi::PyModule_Create(&mut MODULE_DEF);

    if module.is_null() {
        return module;
    }

    let module = match PyObject::from_owned_ptr(py, module).cast_into::<PyModule>(py) {
        Ok(m) => m,
        Err(e) => {
            PyErr::from(e).restore(py);
            return std::ptr::null_mut();
        }
    };

    // We could inline init(), but then we'd need to do error handling multiple times.
    match init(py, &module) {
        Ok(()) => module.into_object().steal_ptr(),
        Err(e) => {
            e.restore(py);
            std::ptr::null_mut()
        }
    }
}
