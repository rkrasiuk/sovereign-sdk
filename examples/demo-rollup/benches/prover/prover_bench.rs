use std::collections::HashMap;
use std::env;
use std::fs::{read_to_string, remove_file, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use const_rollup_config::{ROLLUP_NAMESPACE_RAW, SEQUENCER_DA_ADDRESS};
use demo_stf::genesis_config::{get_genesis_config, GenesisPaths};
use demo_stf::runtime::Runtime;
use log4rs::config::{Appender, Config, Root};
use regex::Regex;
use risc0::ROLLUP_ELF;
use sov_celestia_adapter::types::{FilteredCelestiaBlock, Namespace};
use sov_celestia_adapter::verifier::address::CelestiaAddress;
use sov_celestia_adapter::verifier::{CelestiaSpec, RollupParams};
use sov_celestia_adapter::CelestiaService;
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::SlotData;
use sov_modules_stf_template::AppTemplate;
use sov_risc0_adapter::host::Risc0Host;
use sov_rollup_interface::da::DaSpec;
use sov_rollup_interface::services::da::DaService;
use sov_rollup_interface::stf::StateTransitionFunction;
use sov_rollup_interface::zk::{Zkvm, ZkvmHost};
use sov_state::{ProverStorage, Storage};
use sov_stf_runner::{from_toml_path, RollupConfig};
use tempfile::TempDir;

fn new_app<Vm: Zkvm, Da: DaSpec>(
    storage_config: sov_state::config::Config,
) -> AppTemplate<DefaultContext, Da, Vm, Runtime<DefaultContext, Da>> {
    let storage =
        ProverStorage::with_config(storage_config).expect("Failed to open prover storage");
    AppTemplate::new(storage.clone())
}

#[derive(Debug)]
struct RegexAppender {
    regex: Regex,
    file: Arc<Mutex<File>>,
}

impl RegexAppender {
    fn new(pattern: &str, file_path: &str) -> Self {
        if Path::new(file_path).exists() {
            remove_file(file_path).expect("Failed to remove existing file");
        }
        let file = Arc::new(Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(file_path)
                .unwrap(),
        ));
        let regex = Regex::new(pattern).unwrap();
        RegexAppender { regex, file }
    }
}

impl log::Log for RegexAppender {
    fn log(&self, record: &log::Record) {
        if let Some(captures) = self.regex.captures(record.args().to_string().as_str()) {
            let mut file_guard = self.file.lock().unwrap();
            if let Some(matched_pc) = captures.get(1) {
                let pc_value_num = u64::from_str_radix(&matched_pc.as_str()[2..], 16).unwrap();
                let pc_value = format!("{}\t", pc_value_num);
                file_guard.write_all(pc_value.as_bytes()).unwrap();
            }
            if let Some(matched_iname) = captures.get(2) {
                let iname = matched_iname.as_str().to_uppercase();
                let iname_value = format!("{}\n", iname);
                file_guard.write_all(iname_value.as_bytes()).unwrap();
            }
        }
    }

    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn flush(&self) {}
}

fn get_config(rollup_trace: &str) -> Config {
    // [942786] pc: 0x0008e564, insn: 0xffc67613 => andi x12, x12, -4
    let regex_pattern = r".*?pc: (0x[0-9a-fA-F]+), insn: .*?=> ([a-z]*?) ";

    let custom_appender = RegexAppender::new(regex_pattern, rollup_trace);

    Config::builder()
        .appender(Appender::builder().build("custom_appender", Box::new(custom_appender)))
        .build(
            Root::builder()
                .appender("custom_appender")
                .build(log::LevelFilter::Trace),
        )
        .unwrap()
}

#[cfg(feature = "bench")]
use sov_risc0_adapter::metrics::GLOBAL_HASHMAP;

// The rollup stores its data in the namespace b"sov-test" on Celestia
const ROLLUP_NAMESPACE: Namespace = Namespace::const_v0(ROLLUP_NAMESPACE_RAW);

#[macro_use]
extern crate prettytable;

use prettytable::Table;

fn print_cycle_averages(metric_map: HashMap<String, (u64, u64)>) {
    let mut metrics_vec: Vec<(String, (u64, u64))> = metric_map
        .iter()
        .map(|(k, (sum, count))| {
            (
                k.clone(),
                (((*sum as f64) / (*count as f64)).round() as u64, *count),
            )
        })
        .collect();

    metrics_vec.sort_by(|a, b| b.1.cmp(&a.1));

    let mut table = Table::new();
    table.add_row(row!["Function", "Average Cycles", "Num Calls"]);
    for (k, (avg, count)) in metrics_vec {
        table.add_row(row![k, format!("{}", avg), format!("{}", count)]);
    }
    table.printstd();
}

fn chain_stats(num_blocks: usize, num_blocks_with_txns: usize, num_txns: usize, num_blobs: usize) {
    let mut table = Table::new();
    table.add_row(row!["Total blocks", num_blocks]);
    table.add_row(row!["Blocks with transactions", num_blocks_with_txns]);
    table.add_row(row!["Number of blobs", num_blobs]);
    table.add_row(row!["Total number of transactions", num_txns]);
    table.add_row(row![
        "Average number of transactions per block",
        ((num_txns as f64) / (num_blocks_with_txns as f64)) as u64
    ]);
    table.printstd();
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    if let Ok(rollup_trace) = env::var("ROLLUP_TRACE") {
        if let Err(e) = log4rs::init_config(get_config(&rollup_trace)) {
            eprintln!("Error initializing logger: {:?}", e);
        }
    }

    let rollup_config_path = "benches/prover/rollup_config.toml".to_string();
    let mut rollup_config: RollupConfig<sov_celestia_adapter::DaServiceConfig> =
        from_toml_path(&rollup_config_path)
            .context("Failed to read rollup configuration")
            .unwrap();

    let mut num_blocks = 0;
    let mut num_blobs = 0;
    let mut num_blocks_with_txns = 0;
    let mut num_total_transactions = 0;

    let temp_dir = TempDir::new().expect("Unable to create temporary directory");

    rollup_config.storage.path = PathBuf::from(temp_dir.path());

    let da_service = CelestiaService::new(
        rollup_config.da.clone(),
        RollupParams {
            namespace: ROLLUP_NAMESPACE,
        },
    )
    .await;

    let storage_config = sov_state::config::Config {
        path: rollup_config.storage.path,
    };

    let mut demo = new_app::<Risc0Host, CelestiaSpec>(storage_config);

    let sequencer_da_address = CelestiaAddress::from_str(SEQUENCER_DA_ADDRESS).unwrap();

    let genesis_config = get_genesis_config(
        sequencer_da_address,
        &GenesisPaths::from_dir("../test-data/genesis/demo-tests"),
        #[cfg(feature = "experimental")]
        Default::default(),
    );
    println!("Starting from empty storage, initialization chain");
    let mut prev_state_root = demo.init_chain(genesis_config);

    let hex_data = read_to_string("benches/prover/blocks.hex").expect("Failed to read data");
    let bincoded_blocks: Vec<FilteredCelestiaBlock> = hex_data
        .lines()
        .map(|line| {
            let bytes = hex::decode(line).expect("Failed to decode hex data");
            bincode::deserialize(&bytes).expect("Failed to deserialize data")
        })
        .collect();

    for height in 2..(bincoded_blocks.len() as u64) {
        num_blocks += 1;
        let mut host = Risc0Host::new(ROLLUP_ELF);
        host.add_hint(prev_state_root);
        println!(
            "Requesting data for height {} and prev_state_root 0x{}",
            height,
            hex::encode(prev_state_root.0)
        );
        let filtered_block = &bincoded_blocks[height as usize];
        let _header_hash = hex::encode(filtered_block.header.header.hash());
        host.add_hint(&filtered_block.header);
        let (mut blob_txs, inclusion_proof, completeness_proof) = da_service
            .extract_relevant_blobs_with_proof(filtered_block)
            .await;

        host.add_hint(&inclusion_proof);
        host.add_hint(&completeness_proof);
        host.add_hint(&blob_txs);

        if !blob_txs.is_empty() {
            num_blobs += blob_txs.len();
        }

        let result = demo.apply_slot(
            &prev_state_root,
            Default::default(),
            &filtered_block.header,
            &filtered_block.validity_condition(),
            &mut blob_txs,
        );
        for r in result.batch_receipts {
            let num_tx = r.tx_receipts.len();
            num_total_transactions += num_tx;
            if num_tx > 0 {
                num_blocks_with_txns += 1;
            }
        }
        // println!("{:?}",result.batch_receipts);

        host.add_hint(&result.witness);

        println!("Skipping prover at block {height} to capture cycle counts\n");
        let _receipt = host
            .run_without_proving()
            .expect("Prover should run successfully");
        println!("==================================================\n");
        prev_state_root = result.state_root;
    }

    #[cfg(feature = "bench")]
    {
        let hashmap_guard = GLOBAL_HASHMAP.lock();
        let metric_map = hashmap_guard.clone();
        let total_cycles = metric_map.get("Cycles per block").unwrap().0;
        println!("\nBlock stats\n");
        chain_stats(
            num_blocks,
            num_blocks_with_txns,
            num_total_transactions,
            num_blobs,
        );
        println!("\nCycle Metrics\n");
        print_cycle_averages(metric_map);
        println!("\nTotal cycles consumed for test: {}\n", total_cycles);
    }

    Ok(())
}
