use ckb_hash::{blake2b_256, new_blake2b};
use ckb_simple_account_layer::{
    run_with_context, CkbBlake2bHasher, Config, RunContext, RunProofResult, RunResult,
};
use ckb_types::{
    bytes::{BufMut, Bytes, BytesMut},
    core::{BlockView, ScriptHashType, TransactionBuilder},
    h256,
    packed::{
        Byte32, BytesOpt, CellInput, CellOutput, OutPoint, Script, ScriptOpt, Transaction,
        WitnessArgs,
    },
    prelude::*,
    H160, H256, U256,
};
use ckb_vm::{
    registers::{A0, A1, A2, A3, A4, A7},
    Error as VMError, Memory, Register, SupportMachine,
};
use sparse_merkle_tree::{default_store::DefaultStore, SparseMerkleTree, H256 as SmtH256};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::error::Error as StdError;

use super::Loader;
use crate::types::{
    h256_to_smth256, parse_log, smth256_to_h256, vm_load_data, vm_load_h160, vm_load_h256,
    vm_load_i32, vm_load_i64, vm_load_u32, vm_load_u8, CallKind, Coinbase, ContractAddress,
    ContractCell, EoaAddress, Program, RunConfig, WitnessData, ALWAYS_SUCCESS_SCRIPT,
    MIN_CELL_CAPACITY, ONE_CKB, SIGHASH_CELL_DEP, SIGHASH_TYPE_HASH,
};

pub struct Runner {
    pub loader: Loader,
    pub run_config: RunConfig,
}

impl Runner {
    pub fn new(loader: Loader, run_config: RunConfig) -> Runner {
        Runner { loader, run_config }
    }

    pub fn static_call(
        &mut self,
        sender: H160,
        destination: ContractAddress,
        input: Bytes,
    ) -> Result<CsalRunContext, Box<dyn StdError>> {
        let meta = self.loader.load_contract_meta(destination.clone())?;
        if meta.destructed {
            return Err(format!("Contract already destructed: {:x}", destination.0).into());
        }
        let program = Program::new_call(
            EoaAddress(sender.clone()),
            sender,
            destination,
            meta.code,
            input,
            false,
        );

        let tip_block = self.loader.load_block(None)?;
        let mut context =
            CsalRunContext::new(self.loader.clone(), self.run_config.clone(), tip_block);
        if let Err(err) = context.run(program) {
            log::warn!("Error: {:?}", err);
            return Err(err);
        }
        // TODO: merge with context
        Ok(context)
    }

    pub fn call(
        &mut self,
        sender: H160,
        destination: ContractAddress,
        input: Bytes,
    ) -> Result<CsalRunContext, Box<dyn StdError>> {
        let meta = self.loader.load_contract_meta(destination.clone())?;
        if meta.destructed {
            return Err(format!("Contract already destructed: {:x}", destination.0).into());
        }
        let program = Program::new_call(
            EoaAddress(sender.clone()),
            sender,
            destination,
            meta.code,
            input,
            false,
        );

        let tip_block = self.loader.load_block(None)?;
        let mut context =
            CsalRunContext::new(self.loader.clone(), self.run_config.clone(), tip_block);
        if let Err(err) = context.run(program) {
            log::warn!("Error: {:?}", err);
            return Err(err);
        }
        Ok(context)
    }

    pub fn create(
        &mut self,
        sender: H160,
        code: Bytes,
    ) -> Result<CsalRunContext, Box<dyn StdError>> {
        let program = Program::new_create(EoaAddress(sender.clone()), sender, code);
        let tip_block = self.loader.load_block(None)?;
        let mut context =
            CsalRunContext::new(self.loader.clone(), self.run_config.clone(), tip_block);
        if let Err(err) = context.run(program) {
            log::warn!("Error: {:?}", err);
            return Err(err);
        }
        Ok(context)
    }
}

pub struct ContractInfo {
    pub address: ContractAddress,
    pub tree: SparseMerkleTree<CkbBlake2bHasher, SmtH256, DefaultStore<SmtH256>>,
    pub code: Bytes,
    // input and selfdestruct can not both empty (invoke selfdestruct in a constructor?)
    pub input: Option<ContractInput>,
    pub selfdestruct: Option<Bytes>,
    pub run_result: RunResult,
    execute_index: usize,
    // (program, logs, return_data, run_proof)
    execute_records: Vec<ExecuteRecord>,
    current_calls: Vec<(ContractAddress, u32)>,
}

#[derive(Clone)]
pub struct ContractInput {
    out_point: OutPoint,
    output: CellOutput,
    data: Bytes,
}

impl ContractInput {
    pub fn new(out_point: OutPoint, output: CellOutput, data: Bytes) -> ContractInput {
        ContractInput {
            out_point,
            output,
            data,
        }
    }
    pub fn cell_input(&self) -> CellInput {
        CellInput::new(self.out_point.clone(), 0)
    }
    pub fn capacity(&self) -> u64 {
        self.output.capacity().unpack()
    }
}

pub struct ExecuteRecord {
    // Initial set
    pub program: Program,
    // Update in syscall
    pub logs: Vec<Bytes>,
    // Update in syscall
    pub return_data: Bytes,
    // Update after run_with_context
    pub run_proof: Bytes,

    pub calls: Vec<(ContractAddress, u32)>,
}

impl ExecuteRecord {
    pub fn new(program: Program) -> ExecuteRecord {
        ExecuteRecord {
            program,
            logs: Vec::new(),
            return_data: Bytes::default(),
            run_proof: Bytes::default(),
            calls: Vec::new(),
        }
    }

    pub fn witness_data(&self, first_program: bool, block_opt: Option<&BlockView>) -> WitnessData {
        // This optmize is for reducing witness size by remove duplicated code field
        let mut program = self.program.clone();
        if !first_program {
            program.code = Bytes::default();
        }
        for (dest, program_index) in &self.calls {
            log::debug!("[call]: ({:x}, {})", dest.0, program_index);
        }
        log::debug!("[run_proof]: {}", hex::encode(&self.run_proof));
        let coinbase = if first_program {
            block_opt.map(Coinbase::new)
        } else {
            None
        };
        WitnessData {
            signature: Bytes::from(vec![0u8; 65]),
            program,
            return_data: self.return_data.clone(),
            selfdestruct: None,
            coinbase,
            calls: self.calls.clone(),
            run_proof: self.run_proof.clone(),
        }
    }
}

impl ContractInfo {
    pub fn new(
        address: ContractAddress,
        input: Option<ContractInput>,
        tree: SparseMerkleTree<CkbBlake2bHasher, SmtH256, DefaultStore<SmtH256>>,
    ) -> ContractInfo {
        ContractInfo {
            address,
            input,
            tree,
            code: Bytes::default(),
            execute_index: 0,
            execute_records: Vec::new(),
            selfdestruct: None,
            run_result: RunResult::default(),
            current_calls: Default::default(),
        }
    }

    // The storage tree root hash
    pub fn storage_root(&self) -> H256 {
        smth256_to_h256(self.tree.root())
    }
    // The contract code hash
    pub fn code_hash(&self) -> H256 {
        assert!(
            !self.code.is_empty(),
            "contract code is empty: {:x}",
            self.address.0
        );
        H256::from_slice(&blake2b_256(self.code.as_ref())[..]).unwrap()
    }

    pub fn output_data(&self) -> Bytes {
        log::debug!(
            "[address]: {:x}, storage_root: {:x}",
            self.address.0,
            self.storage_root(),
        );
        ContractCell::new(self.storage_root(), self.code_hash()).serialize()
    }

    pub fn return_data(&self) -> Bytes {
        self.execute_records[self.execute_records.len() - 1]
            .return_data
            .clone()
    }

    pub fn get_logs(&self) -> Result<Vec<(Vec<H256>, Bytes)>, String> {
        self.execute_records
            .iter()
            .try_fold(Vec::new(), |mut all_logs, record| {
                for log_data in &record.logs {
                    all_logs.push(parse_log(log_data)?);
                }
                Ok(all_logs)
            })
    }

    // Serialize all call records to WitnessArgs
    pub fn witness_data(&self, block_opt: Option<&BlockView>) -> WitnessArgs {
        let mut witness_data_vec: Vec<WitnessData> = self
            .execute_records
            .iter()
            .enumerate()
            .map(|(idx, record)| record.witness_data(idx == 0, block_opt))
            .collect();
        witness_data_vec[self.execute_records.len() - 1].selfdestruct = self
            .selfdestruct
            .as_ref()
            .map(|data| H160::from_slice(data.as_ref()).unwrap());
        for witness_data in &witness_data_vec {
            let program = &witness_data.program;
            log::debug!("[address]    : {:x}", self.address.0);
            log::debug!("[kind]       : {:?}", program.kind);
            log::debug!("[sender]     : {:x}", program.sender);
            log::debug!("[destination]: {:x}", program.destination.0);
            log::debug!("[input]      : {}", hex::encode(&program.input));
            log::debug!("[code]       : {}", hex::encode(&program.code));
            log::debug!("----------------------------------------------");
        }
        log::info!(
            "contract {} have {} program",
            self.execute_records[0].program.destination.0,
            witness_data_vec.len()
        );
        let mut data = BytesMut::default();
        for witness_data in witness_data_vec {
            data.put(witness_data.serialize().as_ref());
        }
        // The end of all programs (just like '\0' of C string)
        data.put(&0u32.to_le_bytes()[..]);
        let data = BytesOpt::new_builder()
            .set(Some(data.freeze().pack()))
            .build();
        if self.is_create() {
            WitnessArgs::new_builder().output_type(data).build()
        } else {
            WitnessArgs::new_builder().input_type(data).build()
        }
    }

    pub fn is_create(&self) -> bool {
        self.execute_records[0].program.is_create()
    }

    pub fn add_record(&mut self, program: Program) {
        if !program.is_create() && self.code.is_empty() {
            self.code = program.code.clone();
        }
        self.execute_records.push(ExecuteRecord::new(program));
        self.execute_index = self.execute_records.len();
    }

    pub fn get_last_call(&self) -> u32 {
        (self.execute_index - 1) as u32
    }

    pub fn current_return_data(&self) -> &Bytes {
        &self.current_record().return_data
    }
    pub fn current_record(&self) -> &ExecuteRecord {
        &self.execute_records[self.execute_index - 1]
    }

    pub fn current_record_mut(&mut self) -> &mut ExecuteRecord {
        &mut self.execute_records[self.execute_index - 1]
    }
}

pub struct CsalRunContext {
    pub loader: Loader,
    pub run_config: RunConfig,
    pub tip_block: BlockView,
    // Save header deps for get_block_hash
    pub header_deps: HashSet<H256>,
    // The transaction origin address
    pub tx_origin: EoaAddress,
    // First fuel input cell
    pub first_fuel_input: Option<(CellInput, u64)>,
    // First contract input cell (when kind == CallKind::CALL || kind == DELEGATECALL)
    pub first_contract_input: Option<ContractInput>,
    // The entrance program
    pub entrance_program: Option<Program>,
    // Current running contract
    contract_index: usize,
    contracts: Vec<(ContractAddress, ContractInfo)>,
    state_changed: bool,
}

impl CsalRunContext {
    pub fn new(loader: Loader, run_config: RunConfig, tip_block: BlockView) -> CsalRunContext {
        CsalRunContext {
            loader,
            run_config,
            tip_block,
            header_deps: HashSet::default(),
            tx_origin: Default::default(),
            first_fuel_input: None,
            first_contract_input: None,
            entrance_program: None,
            contract_index: 0,
            contracts: Vec::new(),
            state_changed: false,
        }
    }

    pub fn is_static(&self) -> bool {
        self.entrance_program
            .as_ref()
            .map(|program| program.flags == 1)
            .unwrap_or(false)
    }

    pub fn build_tx(&mut self) -> Result<Transaction, Box<dyn StdError>> {
        if self.is_static() && self.state_changed {
            return Err(String::from("state changed in static call").into());
        }
        if !self.is_static() && !self.state_changed {
            return Err(String::from("state not changed in create/call").into());
        }

        let tx_fee = ONE_CKB;
        // Setup cell_deps
        // TODO: fill load all inputs' headers as dependencies
        let cell_deps = vec![
            SIGHASH_CELL_DEP.clone(),
            self.run_config.type_dep.clone(),
            self.run_config.lock_dep.clone(),
        ];

        // Collect inputs (stage 0)
        let other_inputs: Vec<CellInput> = self
            .contracts
            .iter()
            .filter_map(|(_, info)| info.input.as_ref().map(|input| input.cell_input()))
            .collect();
        let first_fuel_capacity: u64 = self
            .first_fuel_input
            .as_ref()
            .map(|(_, capacity)| *capacity)
            .unwrap_or_default();
        let other_total_capacity: u64 = self
            .contracts
            .iter()
            .filter_map(|(_, info)| info.input.as_ref().map(|input| input.capacity()))
            .sum();
        let total_input_capacity = first_fuel_capacity + other_total_capacity;

        // Collect outputs/outputs_data
        let (mut outputs, mut outputs_data): (Vec<CellOutput>, Vec<Bytes>) = self
            .contracts
            .iter()
            .map(|(address, info)| {
                if let Some(ref selfdestruct_target) = info.selfdestruct {
                    let capacity = info.input.as_ref().map(|input| input.capacity()).unwrap();
                    let output = CellOutput::new_builder()
                        .lock(
                            Script::new_builder()
                                .code_hash(SIGHASH_TYPE_HASH.pack())
                                .hash_type(ScriptHashType::Type.into())
                                .args(selfdestruct_target.pack())
                                .build(),
                        )
                        .capacity(capacity.pack())
                        .build();
                    (output, Bytes::default())
                } else {
                    let output = info
                        .input
                        .as_ref()
                        .map(|input| input.output.clone())
                        .unwrap_or_else(|| {
                            // Create contract
                            let output_capacity = 200 * ONE_CKB;
                            let contract_lock_script = ALWAYS_SUCCESS_SCRIPT.clone();
                            let contract_type_script = self
                                .run_config
                                .type_script
                                .clone()
                                .as_builder()
                                .args(Bytes::from(address.0.as_bytes().to_vec()).pack())
                                .build();
                            CellOutput::new_builder()
                                .type_(
                                    ScriptOpt::new_builder()
                                        .set(Some(contract_type_script))
                                        .build(),
                                )
                                .lock(contract_lock_script)
                                .capacity(output_capacity.pack())
                                .build()
                        });
                    (output, info.output_data())
                }
            })
            .unzip();
        let total_output_capacity: u64 = outputs
            .iter()
            .map(|output| Unpack::<u64>::unpack(&output.capacity()))
            .sum();

        // Collect more fuel cells
        let mut inputs = vec![];
        if let Some((input, _)) = self.first_fuel_input.clone() {
            inputs.push(input);
        }
        inputs.extend(other_inputs);
        let capacity_left = if total_output_capacity + tx_fee > total_input_capacity {
            let rest_capacity = total_output_capacity + tx_fee - total_input_capacity;
            let (live_cells, actual_rest_capacity) = self
                .loader
                .collect_cells(self.tx_origin.clone(), rest_capacity)?;
            inputs.extend(
                live_cells
                    .into_iter()
                    .map(|out_point| CellInput::new(out_point, 0)),
            );
            actual_rest_capacity - rest_capacity
        } else {
            total_input_capacity - (total_output_capacity + tx_fee)
        };
        if capacity_left >= MIN_CELL_CAPACITY {
            let sender_args = Bytes::from(self.tx_origin.0.as_bytes().to_vec());
            let output = CellOutput::new_builder()
                .lock(
                    Script::new_builder()
                        .code_hash(SIGHASH_TYPE_HASH.pack())
                        .hash_type(ScriptHashType::Type.into())
                        .args(sender_args.pack())
                        .build(),
                )
                .capacity(capacity_left.pack())
                .build();
            outputs.push(output);
            outputs_data.push(Bytes::default());
        }

        let tip_hash: H256 = self.tip_block.hash().unpack();
        let mut header_deps = self
            .loader
            .load_header_deps(&inputs)?
            .into_iter()
            .chain(self.header_deps.clone())
            .filter(|hash| hash != &tip_hash)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        header_deps.insert(0, tip_hash);

        // Collect witnesses, and give them correct positions
        let mut input_index = if self.first_fuel_input.is_some() {
            1
        } else {
            0
        };
        let mut witnesses_data = vec![(None, None); self.contracts.len()];
        for (output_index, (_, info)) in self.contracts.iter().enumerate() {
            // entrance contract
            let block_opt = if output_index == 0 {
                Some(&self.tip_block)
            } else {
                None
            };
            let witness_data = info.witness_data(block_opt);
            if info.is_create() {
                witnesses_data[output_index].1 = Some(witness_data);
            } else {
                witnesses_data[input_index].0 = Some(witness_data);
                input_index += 1;
            }
        }

        let mut witnesses: Vec<Bytes> = Vec::new();
        for (input_witness, output_witness) in witnesses_data {
            if input_witness.is_none() && output_witness.is_none() {
                break;
            }
            let mut witness_builder = WitnessArgs::new_builder();
            if let Some(witness_data) = input_witness {
                witness_builder = witness_builder.input_type(witness_data.input_type());
            }
            if let Some(witness_data) = output_witness {
                witness_builder = witness_builder.output_type(witness_data.output_type());
            }
            witnesses.push(witness_builder.build().as_bytes());
        }

        let tx = TransactionBuilder::default()
            .header_deps(header_deps.into_iter().map(|hash| hash.pack()))
            .cell_deps(cell_deps.pack())
            .inputs(inputs.pack())
            .outputs(outputs.pack())
            .outputs_data(outputs_data.pack())
            .witnesses(witnesses.pack())
            .build();
        Ok(tx.data())
    }

    // Add CALLCODE/DELEGATECALL program for callee
    pub fn add_special_call(&mut self, program: Program) -> Result<(), Box<dyn StdError>> {
        let info_address = program.destination.clone();
        let (contract_input_opt, tree) = self
            .get_contract_info(&info_address)
            .map::<Result<_, String>, _>(|info| {
                let input_opt: Option<ContractInput> = info.input.clone();
                let tree = SparseMerkleTree::new(*info.tree.root(), info.tree.store().clone());
                Ok((input_opt, tree))
            })
            .unwrap_or_else(|| {
                let change = self.loader.load_latest_contract_change(
                    info_address.clone(),
                    None,
                    false,
                    false,
                )?;
                let (output, data) = self
                    .loader
                    .load_contract_live_cell(change.tx_hash.clone(), change.output_index)?;
                let input = ContractInput::new(change.out_point(), output, data);
                Ok((Some(input), change.merkle_tree()))
            })?;

        let empty_run_proof = Bytes::from(RunProofResult::default().serialize_pure().unwrap());
        log::debug!("empty_run_proof: {}", hex::encode(&empty_run_proof));
        if let Some(contract_index) = self.get_contract_index(&info_address) {
            self.contract_index = contract_index;
            let info = &mut self.contracts[contract_index].1;
            info.add_record(program);
            info.current_record_mut().run_proof = empty_run_proof;
        } else {
            self.contract_index = self.contracts.len();
            let mut info = ContractInfo::new(info_address.clone(), contract_input_opt, tree);
            info.add_record(program);
            info.current_record_mut().run_proof = empty_run_proof;
            self.contracts.push((info_address, info));
        }
        Ok(())
    }

    pub fn run(&mut self, mut program: Program) -> Result<(), Box<dyn StdError>> {
        if self.contracts.is_empty() {
            self.set_entrance_program(program.clone())?;
        }

        let mut info_address = match program.kind {
            CallKind::CALL | CallKind::CREATE | CallKind::CREATE2 => program.destination.clone(),
            CallKind::CALLCODE | CallKind::DELEGATECALL => self.current_contract_address().clone(),
        };

        if program.is_create() {
            self.state_changed = true;
        }
        let (contract_input_opt, tree) = if program.is_create() {
            (None, SparseMerkleTree::default())
        } else {
            self.get_contract_info(&info_address)
                .map::<Result<_, String>, _>(|info| {
                    let input_opt: Option<ContractInput> = info.input.clone();
                    let tree = SparseMerkleTree::new(*info.tree.root(), info.tree.store().clone());
                    Ok((input_opt, tree))
                })
                .unwrap_or_else(|| {
                    let change = self.loader.load_latest_contract_change(
                        info_address.clone(),
                        None,
                        false,
                        false,
                    )?;
                    let (output, data) = self
                        .loader
                        .load_contract_live_cell(change.tx_hash.clone(), change.output_index)?;
                    let input = ContractInput::new(change.out_point(), output, data);
                    Ok((Some(input), change.merkle_tree()))
                })?
        };
        let mut new_tree = SparseMerkleTree::new(*tree.root(), tree.store().clone());
        // FIXME: The output_index is wrong
        let destination = self.destination(&program, self.contracts.len() as u64);
        if program.is_create() {
            info_address = destination.clone();
            program.destination = destination;
        }

        log::debug!("[contract]: {:x}", info_address.0);
        if let Some(contract_index) = self.get_contract_index(&info_address) {
            self.contract_index = contract_index;
            let info = &mut self.contracts[contract_index].1;
            info.add_record(program.clone());
        } else {
            self.contract_index = self.contracts.len();
            let mut info = ContractInfo::new(info_address.clone(), contract_input_opt, tree);
            info.add_record(program.clone());
            self.contracts.push((info_address, info));
        }

        let program_data = WitnessData::new(program.clone()).program_data();
        log::debug!(
            "[length]: {}",
            hex::encode(&(program_data.len() as u32).to_le_bytes()[..])
        );
        log::debug!("[binary]: {}", hex::encode(program_data.as_ref()));
        let saved_execute_index = self.current_contract_info().execute_index;
        let config = Config::from(&self.run_config);
        let _result = match run_with_context(&config, &new_tree, &program_data, self) {
            Ok(result) => result,
            Err(err) => {
                log::warn!("Error: {:?}", err);
                return Err(err);
            }
        };
        let current_info = self.current_contract_info_mut();
        current_info.execute_index = saved_execute_index;

        if program.kind.is_special_call() {
            current_info.current_record_mut().run_proof =
                Bytes::from(RunProofResult::default().serialize_pure().unwrap());
        } else {
            let run_result = std::mem::take(&mut current_info.run_result);
            let proof = run_result.generate_proof(&new_tree)?;
            print_proof(&proof);
            run_result.commit(&mut new_tree).unwrap();
            // Update storage tree
            current_info.tree = new_tree;
            // Update run_proof
            current_info.current_record_mut().run_proof =
                Bytes::from(proof.serialize_pure().unwrap());
            current_info.current_record_mut().calls =
                std::mem::take(&mut current_info.current_calls);
            if !run_result.write_values.is_empty() {
                self.state_changed = true;
            }
        }
        Ok(())
    }

    pub fn set_entrance_program(&mut self, program: Program) -> Result<(), Box<dyn StdError>> {
        if program.is_create() {
            let (live_cells, total_capacity) = self
                .loader
                .collect_cells(EoaAddress(program.sender.clone()), MIN_CELL_CAPACITY)?;
            if live_cells.is_empty() {
                return Err(format!("Can found live cell from {:x}", program.sender).into());
            }
            // If live_cells is not empty, there must be only one live cell.
            self.first_fuel_input =
                Some((CellInput::new(live_cells[0].clone(), 0), total_capacity));
        } else {
            let latest_change = self.loader.load_latest_contract_change(
                program.destination.clone(),
                None,
                false,
                false,
            )?;

            let out_point = OutPoint::new(latest_change.tx_hash.pack(), latest_change.output_index);
            let (contract_live_cell, latest_contract_data) = self.loader.load_contract_live_cell(
                latest_change.tx_hash.clone(),
                latest_change.output_index,
            )?;
            self.first_contract_input = Some(ContractInput::new(
                out_point,
                contract_live_cell,
                latest_contract_data,
            ));
        }
        log::info!("> tx_origin: {:x}", program.sender);
        self.tx_origin = EoaAddress(program.sender.clone());
        self.entrance_program = Some(program);
        Ok(())
    }

    pub fn first_cell_input(&self) -> (CellInput, u64) {
        if let Some(ref input) = self.first_contract_input {
            (input.cell_input(), input.capacity())
        } else if let Some(ref input) = self.first_fuel_input {
            input.clone()
        } else {
            // See set_entrance_program()
            panic!("first_fuel_input and first_contract_input can not both None");
        }
    }

    pub fn destination(&self, program: &Program, output_index: u64) -> ContractAddress {
        if program.is_create() {
            let type_id_args = {
                let mut blake2b = new_blake2b();
                blake2b.update(self.first_cell_input().0.as_slice());
                blake2b.update(&output_index.to_le_bytes());
                let mut ret = [0; 32];
                blake2b.finalize(&mut ret);
                Bytes::from(ret[0..20].to_vec())
            };
            H160::from_slice(type_id_args.as_ref())
                .map(ContractAddress)
                .unwrap()
        } else {
            program.destination.clone()
        }
    }

    pub fn is_create(&self) -> bool {
        self.contracts[0].1.is_create()
    }
    pub fn entrance_contract(&self) -> ContractAddress {
        self.contracts[0].0.clone()
    }
    pub fn entrance_info(&self) -> &ContractInfo {
        &self.contracts[0].1
    }
    pub fn created_contracts(&self) -> Vec<ContractAddress> {
        self.contracts
            .iter()
            .filter(|(_, info)| info.is_create())
            .map(|(addr, _)| addr.clone())
            .collect()
    }
    pub fn destructed_contracts(&self) -> Vec<ContractAddress> {
        self.contracts
            .iter()
            .filter(|(_, info)| info.selfdestruct.is_some())
            .map(|(addr, _)| addr.clone())
            .collect()
    }
    pub fn get_logs(&self) -> Result<Vec<(ContractAddress, Vec<H256>, Bytes)>, String> {
        self.contracts
            .iter()
            .try_fold(Vec::new(), |mut all_logs, (addr, info)| {
                let logs_iter = info.get_logs().map(|logs| {
                    logs.into_iter()
                        .map(|(topics, data)| (addr.clone(), topics, data))
                })?;
                all_logs.extend(logs_iter);
                Ok(all_logs)
            })
    }

    pub fn get_contract_code(&self, address: &ContractAddress) -> Result<Bytes, String> {
        self.get_contract_info(address)
            .map(|info| info.code.clone())
            .filter(|code| !code.is_empty())
            .map(Ok)
            .unwrap_or_else(|| {
                self.loader
                    .load_contract_meta(address.clone())
                    .map(|meta| meta.code)
            })
    }

    pub fn get_contract_index(&self, address: &ContractAddress) -> Option<usize> {
        self.contracts.iter().position(|(addr, _)| addr == address)
    }
    pub fn get_contract_info(&self, address: &ContractAddress) -> Option<&ContractInfo> {
        self.get_contract_index(address)
            .map(|index| &self.contracts[index].1)
    }
    pub fn current_contract_address(&self) -> &ContractAddress {
        &self.contracts[self.contract_index].0
    }
    pub fn current_contract_info(&self) -> &ContractInfo {
        &self.contracts[self.contract_index].1
    }
    pub fn current_contract_info_mut(&mut self) -> &mut ContractInfo {
        &mut self.contracts[self.contract_index].1
    }
}

impl<Mac: SupportMachine> RunContext<Mac> for CsalRunContext {
    fn ecall(&mut self, machine: &mut Mac) -> Result<bool, VMError> {
        let code = machine.registers()[A7].to_u64();
        match code {
            // ckb_debug
            2177 => {
                let mut addr = machine.registers()[A0].to_u64();
                let mut buffer = Vec::new();

                loop {
                    let byte = machine
                        .memory_mut()
                        .load8(&Mac::REG::from_u64(addr))?
                        .to_u8();
                    if byte == 0 {
                        break;
                    }
                    buffer.push(byte);
                    addr += 1;
                }

                let s = String::from_utf8(buffer).map_err(|_| VMError::ParseError)?;
                log::debug!("ckb_debug: {}", s);
                Ok(true)
            }

            // insert
            3073 => {
                let key_address = machine.registers()[A0].to_u64();
                let key = vm_load_h256(machine, key_address)?;
                let value_address = machine.registers()[A1].to_u64();
                let value = vm_load_h256(machine, value_address)?;
                log::debug!("[set_storage] key={:x}, value={:x}", key, value);
                self.current_contract_info_mut()
                    .run_result
                    .write_values
                    .insert(h256_to_smth256(&key), h256_to_smth256(&value));
                machine.set_register(A0, Mac::REG::from_u64(0));
                Ok(true)
            }
            // fetch
            3074 => {
                let key_address = machine.registers()[A0].to_u64();
                let key = vm_load_h256(machine, key_address)?;
                let value_address = machine.registers()[A1].to_u64();
                log::debug!("[get_storage] key {:x}", key);

                let smth256_key = h256_to_smth256(&key);
                let info = self.current_contract_info_mut();
                let value = match info.run_result.write_values.get(&smth256_key) {
                    Some(value) => *value,
                    None => {
                        let tree_value = info
                            .tree
                            .get(&smth256_key)
                            .map_err(|_| VMError::Unexpected)?;
                        if tree_value != SmtH256::default() {
                            info.run_result.read_values.insert(smth256_key, tree_value);
                        }
                        tree_value
                    }
                };
                machine
                    .memory_mut()
                    .store_bytes(value_address, value.as_slice())?;
                Ok(true)
            }
            // return
            3075 => {
                let data_address = machine.registers()[A0].to_u64();
                let data_length = machine.registers()[A1].to_u32();
                let data = vm_load_data(machine, data_address, data_length)?;
                log::debug!("return_data: {}", hex::encode(&data));
                let info = self.current_contract_info_mut();
                info.current_record_mut().return_data = data.clone().into();
                if info.is_create() && info.execute_records.len() == 1 {
                    log::debug!("update code for contract: {:x}", info.address.0);
                    info.code = data.into();
                }
                Ok(true)
            }
            // LOG{0,1,2,3,4}
            3076 => {
                let data_address = machine.registers()[A0].to_u64();
                let data_length = machine.registers()[A1].to_u32();
                let data = vm_load_data(machine, data_address, data_length)?;
                self.current_contract_info_mut()
                    .current_record_mut()
                    .logs
                    .push(data.into());
                Ok(true)
            }
            // SELFDESTRUCT
            3077 => {
                let data_address = machine.registers()[A0].to_u64();
                let data_length = machine.registers()[A1].to_u32();
                let data = vm_load_data(machine, data_address, data_length)?;
                if self.current_contract_info().selfdestruct.is_some() {
                    panic!("selfdestruct twice: {:?}", self.current_contract_address());
                }
                self.current_contract_info_mut().selfdestruct = Some(data.into());
                self.state_changed = true;
                Ok(true)
            }
            // CALL
            3078 => {
                let mut msg_data_address = machine.registers()[A1].to_u64();
                let kind_value: u8 = vm_load_u8(machine, msg_data_address)?;
                msg_data_address += 1;
                let flags: u32 = vm_load_u32(machine, msg_data_address)?;
                msg_data_address += 4;
                let depth: i32 = vm_load_i32(machine, msg_data_address)?;
                msg_data_address += 4;
                let _gas: i64 = vm_load_i64(machine, msg_data_address)?;
                msg_data_address += 8;
                let destination: H160 = vm_load_h160(machine, msg_data_address)?;
                msg_data_address += 20;
                let sender: H160 = vm_load_h160(machine, msg_data_address)?;
                msg_data_address += 20;
                let input_size: u32 = vm_load_u32(machine, msg_data_address)?;
                msg_data_address += 4;
                let input_data: Vec<u8> = vm_load_data(machine, msg_data_address, input_size)?;
                msg_data_address += input_size as u64;
                let _value: H256 = vm_load_h256(machine, msg_data_address)?;
                msg_data_address += 32;
                let _create2_salt = vm_load_h256(machine, msg_data_address)?;

                let destination = ContractAddress(destination);
                let kind = CallKind::try_from(kind_value).unwrap();
                log::debug!("kind: {:?}, flags: {}, depth: {}, destination: {:x}, sender: {:x}, input_data: {}",
                            kind, flags, depth, destination.0, sender, hex::encode(&input_data));

                if kind == CallKind::DELEGATECALL && sender != self.tx_origin.0 {
                    log::error!(
                        "Invalid DELEGATECALL: sender={:x}, tx_origin={:x}",
                        sender,
                        self.tx_origin.0
                    );
                    return Err(VMError::Unexpected);
                }

                let (code, input) = match kind {
                    CallKind::CREATE | CallKind::CREATE2 => {
                        (Bytes::from(input_data), Bytes::default())
                    }
                    CallKind::CALL | CallKind::DELEGATECALL | CallKind::CALLCODE => {
                        let code = self.get_contract_code(&destination).map_err(|_err| {
                            log::warn!("load contract code failed: {:x}", destination.0);
                            VMError::IO(std::io::ErrorKind::InvalidInput)
                        })?;
                        (code, Bytes::from(input_data))
                    }
                };
                let program = Program {
                    kind,
                    flags,
                    depth: depth as u32,
                    tx_origin: self.tx_origin.clone(),
                    sender,
                    destination,
                    code,
                    input,
                };
                let saved_contract_index = self.contract_index;
                let destination = self.destination(&program, self.contracts.len() as u64);
                self.run(program.clone())
                    .map_err(|_err| VMError::Unexpected)?;
                // Must after run the program
                if kind.is_special_call() {
                    self.add_special_call(program)
                        .map_err(|_err| VMError::Unexpected)?;
                };
                self.contract_index = saved_contract_index;
                let info_address = if kind.is_special_call() {
                    self.current_contract_address().clone()
                } else {
                    destination.clone()
                };
                let dest_return_data = self
                    .get_contract_info(&info_address)
                    .expect("get contract info")
                    .current_return_data()
                    .clone();
                let dest_program_index = self
                    .get_contract_info(&destination)
                    .expect("get contract info")
                    .get_last_call();

                log::debug!("dest_program_index: {}", dest_program_index);
                self.current_contract_info_mut()
                    .current_calls
                    .push((destination.clone(), dest_program_index));
                let create_address = if kind.is_create() {
                    destination
                } else {
                    ContractAddress(H160::default())
                };

                // Store return_data to VM memory
                let result_data_address = machine.registers()[A0].to_u64();
                let mut result_data = BytesMut::default();
                result_data.put(&(dest_return_data.len() as u32).to_le_bytes()[..]);
                result_data.put(dest_return_data.as_ref());
                result_data.put(create_address.0.as_bytes());
                machine
                    .memory_mut()
                    .store_bytes(result_data_address, result_data.as_ref())?;
                machine.set_register(A0, Mac::REG::from_u8(0));
                Ok(true)
            }
            // get code size
            3079 => {
                let address_ptr = machine.registers()[A0].to_u64();
                let address: H160 = vm_load_h160(machine, address_ptr)?;
                let code_size_ptr = machine.registers()[A1].to_u64();
                let code = self
                    .get_contract_code(&ContractAddress(address.clone()))
                    .map_err(|_err| {
                        log::warn!("load contract code failed: {:x}", address);
                        VMError::IO(std::io::ErrorKind::InvalidInput)
                    })?;
                let code_size: u32 = code.len() as u32;
                log::debug!("code size: {}", code_size);
                machine
                    .memory_mut()
                    .store_bytes(code_size_ptr, &code_size.to_le_bytes()[..])?;
                machine.set_register(A0, Mac::REG::from_u8(0));
                Ok(true)
            }
            // copy code
            3080 => {
                let address_ptr = machine.registers()[A0].to_u64();
                let code_offset = machine.registers()[A1].to_u32() as usize;
                let buffer_data_ptr = machine.registers()[A2].to_u64();
                let buffer_size = machine.registers()[A3].to_u32() as usize;
                let done_size_ptr = machine.registers()[A4].to_u64();

                let address: H160 = vm_load_h160(machine, address_ptr)?;
                let code = self
                    .get_contract_code(&ContractAddress(address.clone()))
                    .map_err(|_err| {
                        log::warn!("load contract code failed: {:x}", address);
                        VMError::IO(std::io::ErrorKind::InvalidInput)
                    })?;
                let done_size = std::cmp::min(code.len() - code_offset, buffer_size);
                let code_slice = &code.as_ref()[code_offset..code_offset + done_size];

                log::debug!("code done size: {}", done_size);
                log::debug!("code slice: {}", hex::encode(code_slice));
                machine
                    .memory_mut()
                    .store_bytes(buffer_data_ptr, code_slice)?;
                machine
                    .memory_mut()
                    .store_bytes(done_size_ptr, &(done_size as u32).to_le_bytes()[..])?;
                machine.set_register(A0, Mac::REG::from_u8(0));
                Ok(true)
            }
            // get block hash by number
            // TODO: block number must less than max(block_number(inputs))?
            3081 => {
                let block_hash_ptr = machine.registers()[A0].to_u64();
                let number = machine.registers()[A1].to_u64();
                let header_view = self.loader.load_header(Some(number)).map_err(|err| {
                    log::warn!("get_block_hash({}), load header failed: {}", number, err);
                    VMError::IO(std::io::ErrorKind::InvalidInput)
                })?;
                let block_hash: H256 = header_view.hash().unpack();
                machine
                    .memory_mut()
                    .store_bytes(block_hash_ptr, block_hash.as_bytes())?;
                machine.set_register(A0, Mac::REG::from_u8(0));
                self.header_deps.insert(block_hash);
                Ok(true)
            }
            // evmc_tx_context {block_number, block_timestamp, difficulty, coinbase, chain_id}
            3082 => {
                let buffer_ptr = machine.registers()[A0].to_u64();
                let mut data = [0u8; 8 + 8 + 32 + 20 + 32];
                let number = self.tip_block.number();
                let timestamp = self.tip_block.timestamp() / 1000;
                let difficulty = self.tip_block.difficulty();
                // TODO: config chain ID
                let chain_id = U256::one();
                let secp_blake160_code_hash: Byte32 =
                    h256!("0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8")
                        .pack();
                // FIXME: only recognize secp_blake160 lock args for now
                let coinbase_opt = self
                    .tip_block
                    .transaction(0)
                    .expect("Cellbase must exists")
                    .output(0)
                    .map(|output| output.lock())
                    .and_then(|cellbase_lock| {
                        if cellbase_lock.hash_type() == ScriptHashType::Type.into()
                            && cellbase_lock.code_hash() == secp_blake160_code_hash
                        {
                            H160::from_slice(cellbase_lock.args().raw_data().as_ref()).ok()
                        } else {
                            None
                        }
                    });
                let coinbase = coinbase_opt.unwrap_or_default();

                log::debug!("number: {}, timestamp: {}", number, timestamp);
                data[0..8].copy_from_slice(&number.to_le_bytes());
                data[8..16].copy_from_slice(&timestamp.to_le_bytes());
                data[16..48].copy_from_slice(&difficulty.to_be_bytes());
                data[48..68].copy_from_slice(coinbase.as_bytes());
                data[68..100].copy_from_slice(&chain_id.to_be_bytes());
                machine.memory_mut().store_bytes(buffer_ptr, &data[..])?;
                machine.set_register(A0, Mac::REG::from_u8(0));
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn print_proof(proof: &RunProofResult) {
    for (i, (key, value)) in proof.read_values.iter().enumerate() {
        log::debug!(
            "read_values[{}]: {} => \n {}",
            i,
            hex::encode(key.as_slice()),
            hex::encode(value.as_slice())
        );
    }
    log::debug!("read_proof: 0x{}", hex::encode(&proof.read_proof[..]));
    for (i, (key, old_value, new_value)) in proof.write_values.iter().enumerate() {
        log::debug!(
            "write_values[{}]: \n {} => new = {}, old = {})",
            i,
            hex::encode(key.as_slice()),
            hex::encode(new_value.as_slice()),
            hex::encode(old_value.as_slice()),
        );
    }
    log::debug!(
        "write_old_proof: 0x{}",
        hex::encode(&proof.write_old_proof[..])
    );
}
