use std::marker::{Send, Sync};

use uuid::Uuid;
use wasmer::{self};
use wasmer_wasix::{virtual_fs, WasiEnv, WasiFunctionEnv};

const START_FUNCTION: &str = "_start";

pub struct WasmProcess {
    function: wasmer::Function,
    env: WasiFunctionEnv,
}

impl WasmProcess {
    pub fn new(wasi_env: WasiFunctionEnv, function: wasmer::Function) -> Self {
        Self {
            function: function,
            env: wasi_env,
        }
    }

    pub fn run(
        &mut self,
        store: &mut wasmer::Store,
    ) -> Result<Box<[wasmer::Value]>, wasmer::RuntimeError> {
        let exit_code = self.function.call(store, &[])?;
        self.env.on_exit(store, None);
        Ok(exit_code)
    }
}

pub struct WasmRuntime {
    store: wasmer::Store,
}

impl WasmRuntime {
    pub fn new() -> Self {
        Self {
            store: wasmer::Store::default(),
        }
    }

    pub fn store_mut(&mut self) -> &mut wasmer::Store {
        &mut self.store
    }

    pub fn build(
        &mut self,
        bytecode: Vec<u8>,
        fs: Box<dyn virtual_fs::FileSystem + Send + Sync>,
    ) -> Result<WasmProcess, Box<dyn std::error::Error>> {
        let module = wasmer::Module::new(&self.store, bytecode).expect("couldn't load WASM module");
        let uuid = Uuid::new_v4();
        let mut wasi_env = WasiEnv::builder(uuid)
            .fs(fs)
            // .args(&["arg1", "arg2"])
            // .env("KEY", "VALUE")
            .finalize(self.store_mut())?;
        let import_object = wasi_env.import_object(self.store_mut(), &module)?;
        let instance = wasmer::Instance::new(self.store_mut(), &module, &import_object)?;

        // // Attach the memory export
        // let memory = instance.exports.get_memory("memory")?;
        // wasi_env.data_mut(&mut store).set_memory(memory.clone());

        wasi_env.initialize(&mut self.store, instance.clone())?;

        let function = instance.exports.get_function(START_FUNCTION)?;

        Ok(WasmProcess::new(wasi_env, function.to_owned()))
    }
}
