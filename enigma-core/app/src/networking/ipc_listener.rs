use crate::networking::messages::*;
use futures::{Future, Stream};
use sgx_types::sgx_enclave_id_t;
use std::sync::Arc;
use tokio_zmq::prelude::*;
use tokio_zmq::{Error, Multipart, Rep};

pub struct IpcListener {
    _context: Arc<zmq::Context>,
    rep_future: Box<Future<Item = Rep, Error = Error>>,
}

impl IpcListener {
    pub fn new(conn_str: &str) -> Self {
        let _context = Arc::new(zmq::Context::new());
        let rep_future = Rep::builder(_context.clone()).bind(conn_str).build();
        IpcListener { _context, rep_future }
    }

    pub fn run<F>(self, f: F) -> impl Future<Item = (), Error = Error>
    where F: Fn(Multipart) -> Multipart {
        self.rep_future.and_then(|rep| {
            let (sink, stream) = rep.sink_stream(25).split();
            stream.map(f).forward(sink).map(|(_stream, _sink)| ())
        })
    }
}

pub fn handle_message(request: Multipart, eid: sgx_enclave_id_t) -> Multipart {
    let mut response = Multipart::new();
    for msg in request {
        let response_msg = match msg.into() {
            IpcRequest::GetRegistrationParams { id } => handling::get_registration_params(id, eid),
            IpcRequest::IdentityChallenge { id, nonce } => handling::identity_challange(id, nonce),
            IpcRequest::GetTip { id, input } => handling::get_tip(id, input),
            IpcRequest::GetTips { id, input } => handling::get_tips(id, input),
            IpcRequest::GetAllTips { id } => handling::get_all_tips(id),
            IpcRequest::GetAllAddrs { id } => handling::get_all_addrs(id),
            IpcRequest::GetDelta { id, input } => handling::get_delta(id, input),
            IpcRequest::GetDeltas { id, input } => handling::get_deltas(id, input),
            IpcRequest::GetContract { id, input } => handling::get_contract(id, input),
            IpcRequest::UpdateNewContract { id, address, bytecode } => handling::update_new_contract(id, address, bytecode),
            IpcRequest::UpdateDeltas { id, deltas } => handling::update_deltas(id, deltas),
            IpcRequest::NewTaskEncryptionKey { id, user_pubkey } => handling::get_dh_user_key(id, user_pubkey, eid),
            IpcRequest::DeploySecretContract { id, input } => handling::deploy_contract(id, input, eid),
            IpcRequest::ComputeTask { id, input } => handling::compute_task(id, input, eid),
            IpcRequest::GetPTTRequest { id, addresses } => handling::get_ptt_req(id, addresses, eid),
        };

        response.push_back(response_msg.unwrap_or_default());
    }
    response
}

// TODO: Make sure that every ? that doesn't require responding with a empty Message is replaced with an appropriate handling
pub(self) mod handling {
    #![allow(dead_code)]
    #![allow(clippy::needless_pass_by_value)]
    use crate::common_u::errors::P2PErr;
    use crate::db::{CRUDInterface, DeltaKey, P2PCalls, Stype, DATABASE};
    use crate::km_u;
    use crate::networking::messages::*;
    use crate::esgx::equote;
    use crate::networking::constants::SPID;
    use crate::wasm_u::wasm;
    use enigma_tools_u::common_u::{FromHex32, LockExpectMutex, Keccak256};
    use enigma_tools_u::esgx::equote as equote_tools;
    use enigma_tools_u::attestation_service::{service::AttestationService, constants::ATTESTATION_SERVICE_URL};
    use failure::Error;
    use hex::{FromHex, ToHex};
    use rmp_serde::Deserializer;
    use serde::Deserialize;
    use serde_json::Value;
    use sgx_types::sgx_enclave_id_t;
    use std::str;
    use zmq::Message;

    pub fn get_registration_params(id: String, eid: sgx_enclave_id_t) -> Result<Message, Error> {
        let sigining_key = equote::get_register_signing_address(eid)?;

        let enc_quote = equote_tools::retry_quote(eid, &SPID, 18)?;
        let service: AttestationService = AttestationService::new(ATTESTATION_SERVICE_URL);
        let response = service.get_report(&enc_quote)?;
        let quote = response.get_quote()?;

        let report_hex = response.result.report_string.as_bytes().to_hex();
        let signature = response.result.signature;

        assert_eq!(str::from_utf8(&quote.report_body.report_data)?.trim_right_matches('\x00'), sigining_key);

        let result = IpcResults::RegistrationParams { sigining_key, report: report_hex, signature };

        Ok(IpcResponse::GetRegistrationParams { id, result }.into())
    }
    /// Not implemented.
    pub fn identity_challange(id: String, nonce: String) -> Result<Message, Error> {
        unimplemented!("identity_challenge: {}, {}", id, nonce)
    }

    pub fn get_tip(id: String, input: String) -> Result<Message, Error> {
        let mut address = [0u8; 32];
        address.copy_from_slice(&input.from_hex_32()?);
        let (tip_key, tip_data) = DATABASE.lock_expect("P2P, GetTip").get_tip::<DeltaKey>(&address)?;

        let key = tip_key.key_type.unwrap_delta();
        let delta = IpcDelta { address: None, key, delta: Some(tip_data.to_hex()) };
        Ok(IpcResponse::GetTip { id, result: delta }.into())

    }

    pub fn get_tips(id: String, input: Vec<String>) -> Result<Message, Error> {
        let mut tips_results = Vec::with_capacity(input.len());
        for data in input {
            let address = data.from_hex_32()?;
            let (tip_key, tip_data) = DATABASE.lock_expect("P2P, GetTips").get_tip::<DeltaKey>(&address)?;
            let delta = IpcDelta::from_delta_key(tip_key, tip_data)?;
            tips_results.push(delta);
        }
        Ok(IpcResponse::GetTips { id, result: IpcResults::Tips(tips_results) }.into())
    }

    pub fn get_all_tips(id: String) -> Result<Message, Error> {
        let tips = DATABASE.lock_expect("P2P GetAllTips").get_all_tips::<DeltaKey>().unwrap_or_default();
        let mut tips_results = Vec::with_capacity(tips.len());
        for (key, data) in tips {
            let delta = IpcDelta::from_delta_key(key, data)?;
            tips_results.push(delta);
        }
        Ok(IpcResponse::GetAllTips { id, result: IpcResults::Tips(tips_results) }.into())
    }

    pub fn get_all_addrs(id: String) -> Result<Message, Error> {
        let addresses: Vec<String> =
            DATABASE.lock_expect("P2P GetAllAddrs").get_all_addresses().unwrap_or_default().iter().map(|addr| addr.to_hex()).collect();
        Ok(IpcResponse::GetAllAddrs { id, result: IpcResults::Addresses(addresses) }.into())
    }

    pub fn get_delta(id: String, input: IpcDelta) -> Result<Message, Error> {
        let address =
            input.address.ok_or(P2PErr { cmd: "GetDelta".to_string(), msg: "Address Missing".to_string() })?.from_hex_32()?;
        let delta_key = DeltaKey::new(address, Stype::Delta(input.key));
        let delta = DATABASE.lock_expect("P2P GetDelta").get_delta(delta_key)?;
        Ok(IpcResponse::GetDelta { id, result: IpcResults::Delta(delta.to_hex()) }.into())
    }

    pub fn get_deltas(id: String, input: Vec<IpcGetDeltas>) -> Result<Message, Error> {
        let mut results = Vec::with_capacity(input.len());
        for data in input {
            let address = data.address.from_hex_32()?;
            let from = DeltaKey::new(address, Stype::Delta(data.from));
            let to = DeltaKey::new(address, Stype::Delta(data.to));

            let db_res = DATABASE.lock_expect("P2P GetDeltas").get_deltas(from, to)?;
            if db_res.is_none() {
                results.push(IpcDelta::default());
                continue; // TODO: Check if this handling makes any sense.
            }
            for (key, data) in db_res.unwrap() {
                let delta = IpcDelta::from_delta_key(key, data)?;
                results.push(delta);
            }
        }

        Ok(IpcResponse::GetDeltas { id, result: IpcResults::Deltas(results) }.into())
    }

    pub fn get_contract(id: String, input: String) -> Result<Message, Error> {
        let address = input.from_hex_32()?;
        let data = DATABASE.lock_expect("P2P GetContract").get_contract(address).unwrap_or_default();
        Ok(IpcResponse::GetContract { id, result: IpcResults::Bytecode(data.to_hex()) }.into())
    }

    pub fn update_new_contract(id: String, address: String, bytecode: String) -> Result<Message, Error> {
        let address_arr = address.from_hex_32()?;
        let bytecode = bytecode.from_hex()?;
        let delta_key = DeltaKey::new(address_arr, Stype::ByteCode);
        DATABASE.lock_expect("P2P UpdateNewContract").force_update(&delta_key, &bytecode)?;
        Ok(IpcResponse::UpdateNewContract { id, address, result: IpcResults::Status(0) }.into())
    }

    pub fn update_deltas(id: String, deltas: Vec<IpcDelta>) -> Result<Message, Error> {
        let mut tuples = Vec::with_capacity(deltas.len());

        for delta in deltas.into_iter() {
            let address =
                delta.address.ok_or(P2PErr { cmd: "UpdateDeltas".to_string(), msg: "Address Missing".to_string() })?.from_hex_32()?;
            let data =
                delta.delta.ok_or(P2PErr { cmd: "UpdateDeltas".to_string(), msg: "Delta Data Missing".to_string() })?.from_hex()?;
            let delta_key = DeltaKey::new(address, Stype::Delta(delta.key));
            tuples.push((delta_key, data));
        }
        let results = DATABASE.lock_expect("P2P UpdateDeltas").insert_tuples(&tuples);
        let mut errors = Vec::with_capacity(tuples.len());

        for ((deltakey, _), res) in tuples.into_iter().zip(results.into_iter()) {
            let mut status = 0;
            if res.is_err() {
                status = 1;
            }
            let key = deltakey.key_type.unwrap_delta();
            let address = deltakey.hash.to_hex();
            let delta = IpcDeltaResult { address, key, status };
            errors.push(delta);
        }
        let result = IpcResults::UpdateDeltasResult { status: 0, errors };
        Ok(IpcResponse::UpdateDeltas { id, result }.into())
    }

    pub fn get_dh_user_key(id: String, _user_pubkey: String, eid: sgx_enclave_id_t) -> Result<Message, Error> {
        let mut user_pubkey = [0u8; 64];
        user_pubkey.clone_from_slice(&_user_pubkey.from_hex().unwrap());

        let (msg, sig) = km_u::get_user_key(eid, &user_pubkey)?;

        let mut des = Deserializer::new(&msg[..]);
        let res: Value = Deserialize::deserialize(&mut des).unwrap();
        let pubkey = serde_json::from_value::<Vec<u8>>(res["pubkey"].clone())?;

        let result = IpcResults::DHKey {dh_key: pubkey.to_hex(), sig: sig.to_hex() };

        Ok(IpcResponse::NewTaskEncryptionKey { id, result }.into())
    }

    pub fn get_ptt_req(id: String, addresses: Vec<String>, eid: sgx_enclave_id_t) -> Result<Message, Error> {
        let mut addresses_arr = Vec::with_capacity(addresses.len());
        for a in addresses {
            addresses_arr.push(a.from_hex_32()?);
        }
        let (data, sig) = km_u::ptt_req(eid, &addresses_arr)?;
        let result = IpcResults::Request { request: data.to_hex(), sig: sig.to_hex() };

        Ok(IpcResponse::GetPTTRequest { id, result }.into())
    }

    pub fn deploy_contract(id: String, input: IpcTask, eid: sgx_enclave_id_t) -> Result<Message, Error> {
        let bytecode = input.pre_code.expect("Bytecode Missing").from_hex()?;
        let contract_address = input.address.from_hex_32()?;
        let enc_args = input.encrypted_args.from_hex()?;
        let constructor = input.encrypted_fn.from_hex()?;
        let mut user_pubkey = [0u8; 64];
        user_pubkey.clone_from_slice(&input.user_pubkey.from_hex()?);
        let result = wasm::deploy(
            eid,
            &bytecode,
            &constructor,
            &enc_args,
            contract_address,
            &user_pubkey,
            input.gas_limit)?;

        let result = IpcResults::TaskResult {
            exe_code: Some(result.output.to_hex()),
            pre_code_hash: Some(bytecode.keccak256().to_hex()),
            used_gas: result.used_gas,
            output: None, // TODO: Return output
            delta: result.delta.into(),
            signature: result.signature.to_hex(),
        };
        Ok( IpcResponse::DeploySecretContract { id, result }.into() )

    }

    pub fn compute_task(id: String, input: IpcTask, eid: sgx_enclave_id_t) -> Result<Message, Error> {
        let enc_args = input.encrypted_args.from_hex()?;
        let address = input.address.from_hex_32()?;
        let callable = input.encrypted_fn.from_hex()?;
        let mut user_pubkey = [0u8; 64];
        user_pubkey.clone_from_slice(&input.user_pubkey.from_hex()?);

        let bytecode = DATABASE.lock_expect("P2P ComputeTask").get_contract(address)?;


        let result = wasm::execute(
            eid,
            &bytecode,
            &callable,
            &enc_args,
            &user_pubkey,
            &address,
            input.gas_limit)?;

        let result = IpcResults::TaskResult {
            exe_code: None,
            pre_code_hash: None,
            used_gas: result.used_gas,
            output: Some(result.output.to_hex()),
            delta: result.delta.into(),
            signature: result.signature.to_hex(),
        };

        Ok( IpcResponse::ComputeTask { id, result }.into() )
    }

}

#[cfg(test)]
mod test {
    use super::*;
    use crate::db::{DeltaKey, P2PCalls, Stype, DATABASE};
    use enigma_tools_u::common_u::LockExpectMutex;
    use serde_json::Value;

    #[ignore]
    #[test]
    fn test_the_listener() {
        let conn = "tcp://*:5556";
        let server = IpcListener::new(conn);
        server
            .run(|mul| {
                println!("{:?}", mul);
                mul
            })
            .wait()
            .unwrap();
    }

    #[ignore]
    #[test]
    fn test_real_listener() {
        let enclave = crate::esgx::general::init_enclave_wrapper().unwrap();
        let provider_db = r#"[{"address":[76,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,33],"key":1,"delta":[150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88]},{"address":[76,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,33],"key":0,"delta":[4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,150,13,149,77,159,158,13,213,171,154,224,241,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194]},{"address":[76,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,33],"key":1,"delta":[135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,150,13,149,77,159,158,13,213,171,154,224,241,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,207,222,86,42,236,92,194,214]},{"address":[76,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,33],"key":2,"delta":[135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,150,13,149,77,159,158,13,213,171,154,224,241,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211]},{"address":[11,214,171,4,67,23,118,195,84,34,103,199,97,21,226,55,220,143,212,246,174,203,51,171,28,30,63,158,131,64,181,200],"key":1,"delta":[11,255,84,134,4,62,190,60,15,43,249,32,21,188,170,27,22,23,8,248,158,176,219,85,175,190,54,199,198,228,198,87,124,33,158,115,60,173,162,16,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,56,90,104,16,241,108,14,126,116,91,106,10,141,122,78,214,148,194,14,31,96,142,178,96,150,52,142,138,37,209,110,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92]},{"address":[11,214,171,4,67,23,118,195,84,34,103,199,97,21,226,55,220,143,212,246,174,203,51,171,28,30,63,158,131,64,181,200],"key":0,"delta":[92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204]},{"address":[13,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,42],"key":1,"delta":[253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31]},{"address":[13,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,42],"key":0,"delta":[88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120]},{"address":[13,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,42],"key":1,"delta":[236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42]}]"#;
        let tips = r#"[{"address":[92,214,171,4,67,94,118,195,84,97,103,199,97,21,226,55,220,143,212,246,174,203,51,171,28,30,63,158,131,79,181,127],"key":10,"delta":[171,255,84,134,4,62,190,60,15,43,249,32,21,188,170,27,22,23,8,248,158,176,219,85,175,190,54,199,198,228,198,87,124,33,158,115,60,173,162,16,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,71,210,240,15,213,37,16,235,133,77,158,220,171,214,255,22,229,31,56,90,104,16,241,108,14,126,116,91,106,10,141,122,78,214,148,194,14,31,96,142,178,96,150,52,142,138,37,209,110,153,185,96,236,44,46,192,138,108,168,91,145,153,60,88,7,229,183,174,187,204,233,54,89,107,16,237,247,66,76,39,82,253,160,2,1,133,210,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,77,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,144,90,20,76,41,98,111,25,84,7,71,84,27,124,190,86,16,136,16,198,76,215,164,228,117,182,238,213,52,253,105,152,215,197,95,244,65,186,140,45,167,114,24,139,199,179,116,105,181]},{"address":[11,214,171,4,67,23,118,195,84,34,103,199,97,21,226,55,220,143,212,246,174,203,51,171,28,30,63,158,131,64,181,200],"key":34,"delta":[11,255,84,134,4,62,190,60,15,43,249,32,21,188,170,27,22,23,8,248,158,176,219,85,175,190,54,199,198,228,198,87,124,33,158,115,60,173,162,16,150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,56,90,104,16,241,108,14,126,116,91,106,10,141,122,78,214,148,194,14,31,96,142,178,96,150,52,142,138,37,209,110,153,185,96,236,44,46,192,138,108,168,91,145,153,60,88,7,229,183,174,187,204,233,54,89,107,16,237,247,66,76,39,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,144,141,221,46,22,81,13,87,209,68,197,189,10,130,182,34,16,198,180,90,20,76,41,98,111,25,84,7,71,84,27,124,190,86,16,136,16,198,76,215,164,228,117,182,238,213,52,253,105,152,215,197,95,244,65,186,140,45,167,114]},{"address":[76,214,171,4,67,23,118,195,84,56,103,199,97,21,226,55,220,54,212,246,174,203,51,171,28,30,63,158,131,64,181,33],"key":0,"delta":[150,13,149,77,159,158,13,213,171,154,224,241,4,42,38,120,66,253,127,201,113,252,246,177,218,155,249,166,68,65,231,208,210,116,89,100,207,92,200,194,48,70,123,210,240,15,213,37,16,235,133,77,158,220,171,33,255,22,229,31,82,253,160,2,1,133,12,135,94,144,211,23,61,150,36,31,55,178,42,128,60,194,192,182,190,227,136,133,252,128,213,88,135,204,213,199,50,191,7,61,104,87,210,127,76,163,11,175,114,207,167,26,249,222,222,73,175,207,222,86,42,236,92,194,214,28,195,236,122,122,12,134,55,41,209,106,172,10,130,139,149,39,196,181,187,55,166,237,215,135,98,90,12,6,72,240,138,112,99,76,55,22,231,223,153,119,15,98,26,77,139,89,64,24,108,137,118,38,142,19,131,220,252,248,212,120,231,26,21,228,246,179,104,207,76,218,88]}]"#;
        let mut provider_db: Value = serde_json::from_str(&provider_db).unwrap();
        let mut tips: Value = serde_json::from_str(&tips).unwrap();

        let data = tips.as_array_mut().unwrap();
        data.append(&mut provider_db.as_array_mut().unwrap());

        let data: Vec<(DeltaKey, Vec<u8>)> = data
            .into_iter()
            .map(|tip| {
                let hash: [u8; 32] = serde_json::from_value(tip["address"].clone()).unwrap();
                let key: u32 = serde_json::from_value(tip["key"].clone()).unwrap();
                let delta_key = DeltaKey { hash, key_type: Stype::Delta(key) };
                let data: Vec<u8> = serde_json::from_value(tip["delta"].clone()).unwrap();
                (delta_key, data)
            })
            .collect();

        for res in DATABASE.lock_expect("test").insert_tuples(&data) {
            res.unwrap();
        }

        let conn = "tcp://*:2456";
        let server = IpcListener::new(conn);
        server.run(|multi| handle_message(multi, enclave.geteid())).wait().unwrap();
    }

}
