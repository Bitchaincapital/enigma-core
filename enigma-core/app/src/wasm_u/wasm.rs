#![allow(dead_code,unused_assignments,unused_variables)]
extern crate sgx_types;
extern crate sgx_urts;

use sgx_types::*;
use failure::Error;
use enigma_tools_u::common_u::errors::WasmError;
use std::iter::FromIterator;

extern {
    fn ecall_deploy(eid: sgx_enclave_id_t,
                 retval: *mut sgx_status_t,
                 bytecode: *const u8, bytecode_len: usize,
                 output: *mut u8, output_len: &mut usize) -> sgx_status_t;
}


/// This module builds Wasm code for contract deployment from the Wasm contract.
/// The contract should be written in rust and then compiled to Wasm with wasm32-unknown-unknown target.
/// The code is based on Parity wasm_utils::cli.

extern crate pwasm_utils as utils;
extern crate parity_wasm;

use self::parity_wasm::elements;
use self::utils::{build, BuildError, SourceTarget};

/*#[derive(Debug)]
pub enum Error {
    Decoding(elements::Error),
    Encoding(elements::Error),
    Build(BuildError),
}*/

/// Builds Wasm code for contract deployment from the Wasm contract.
/// Gets byte vector with Wasm code.
/// Writes created code to a file constructor.wasm in a current directory.
/// This code is based on https://github.com/paritytech/wasm-utils/blob/master/cli/build/main.rs#L68
/// The parameters' values to build function are default parameters as they appear in the original code.
pub fn build_constructor(wasm_code: &Vec<u8>) -> Result<Vec<u8>, Error> {

    let module = parity_wasm::deserialize_buffer(wasm_code)?;
       // .map_err(|e| Error::Decoding(e))?;

    let (module, ctor_module) = match build(
        module,
        SourceTarget::Unknown,
        None,
        &Vec::new(),
        false,
        "49152".parse().expect("New stack size is not valid u32"),
        false,
    ){
        Ok(v) => v,
        Err(e) => panic!(""),
    };

    let result;

    if let Some(ctor_module) = ctor_module {
        result = parity_wasm::serialize(ctor_module);/*.map_err(Error::Encoding)*/
    } else {
        result = parity_wasm::serialize(module);/*.map_err(Error::Encoding)*/
    }

    match result{
        Ok(v) => Ok(v),
        Err(e) => panic!(""),
    }
}


const MAX_EVM_RESULT: usize = 100000;
pub fn deploy(eid: sgx_enclave_id_t,  bytecode: Vec<u8>)-> Result<Vec<u8>,Error>{
    let mut deploy_bytecode = build_constructor(&bytecode)?;
    let mut out = vec![0u8; MAX_EVM_RESULT];
    let slice = out.as_mut_slice();
    let mut retval: sgx_status_t = sgx_status_t::SGX_SUCCESS;
    let mut output_len: usize = 0;

    let result = unsafe {
        ecall_deploy(eid,
                  &mut retval,
                  deploy_bytecode.as_ptr() as *const u8,
                  deploy_bytecode.len(),
                  slice.as_mut_ptr() as *mut u8,
                  &mut output_len)
    };
    let part = Vec::from_iter(slice[0..output_len].iter().cloned());
    println!("{:?}", part);
    Ok(part)
}

#[cfg(test)]
pub mod tests {
    #![allow(dead_code, unused_assignments, unused_variables)]

    use esgx;
    use std::fs::File;
    use std::io::Read;
    use sgx_urts::SgxEnclave;
    use wasm_u::wasm;

    fn init_enclave() -> SgxEnclave{
        let enclave = match esgx::general::init_enclave_wrapper() {
            Ok(r) => {
                println!("[+] Init Enclave Successful {}!", r.geteid());
                r
            }
            Err(x) => {
                panic!("[-] Init Enclave Failed {}!", x.as_str());
            }
        };
        enclave
    }

    #[test]
    pub fn contract() {
        let mut f = File::open("../../examples/eng_wasm_contracts/simplest/target/wasm32-unknown-unknown/release/contract.wasm").unwrap();
        let mut wasm_code = Vec::new();
        f.read_to_end(&mut wasm_code);
        let enclave = init_enclave();
        wasm::deploy(enclave.geteid(), wasm_code);
    }
}