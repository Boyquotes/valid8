use std::{collections::{HashMap, HashSet}, fs::{create_dir_all, File}, io::{Read, Write}, path::{Path, PathBuf}, str::FromStr};
use anyhow::Result;
use dialoguer::Select;
use serde::{Serialize, Deserialize};
use anyhow::anyhow;

use serde_json::Value;
use solana_ledger::{
    blockstore::create_new_ledger, 
    blockstore_options::LedgerColumnOptions,
};

use solana_runtime::genesis_utils::create_genesis_config_with_leader_ex;

use solana_sdk::{
    account::AccountSharedData,
    account_utils::StateMut,
    bpf_loader_upgradeable::UpgradeableLoaderState,
    epoch_schedule::EpochSchedule,
    fee_calculator::FeeRateGovernor,
    native_token::sol_to_lamports,
    pubkey::Pubkey,
    rent::Rent,
    signature::{write_keypair_file, Keypair},
    signer::Signer
};

use crate::{common::{
        helpers, project_name::ProjectName, AccountSchema, Network
    }, 
    config::ConfigJson,
    serialization::b58,
};
const MAX_GENESIS_ARCHIVE_UNPACKED_SIZE: u64 = 10 * 1024 * 1024; // 10 MiB from testvalidator source
/*

    Valid8Context is responsible for managing configuration, dependencies, overrides and atomically saving these changes to our config file.

    Upon startup it will:
    - Check for a valid valid8.json config and try to load it into memory with serde, or
    - If a config file doesn't exist, initialize a new one for us.
    - Check for a local .valid8 directory and ensure it is writable, or
    - Creates the .valid8 directory if it doesn't exist.
    - Loads all IDLs and programs for accounts/programs in our config file

*/

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Valid8Context {
    pub project_name: ProjectName,
    pub networks: HashSet<Network>,
    pub programs: Vec<AccountSchema>,
    pub accounts: Vec<AccountSchema>,
    pub overrides: Option<Vec<Override>>,
    pub idls: Vec<String>,
    pub compose: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct Override {
    #[serde(with = "b58")]
    pub pubkey: Pubkey,
    pub edit_fields: Vec<EditField>,
}

impl Override {
    pub fn new(pubkey: Pubkey, edit_field: EditField) -> Self {
        Self { pubkey: pubkey, edit_fields: vec![edit_field] }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum EditField{
    #[serde(with = "b58")]
    Owner(Pubkey),
    #[serde(with = "b58")]
    UpgradeAuthority(Pubkey),
    Lamports(u64),
    Data(Value)
}

impl From<ConfigJson> for Valid8Context {
    fn from(value: ConfigJson) -> Self {

        // Try to read accounts from disc, or return with default empty vector
        let programs = value.programs.iter()
            .map(|(pubkey, _)| helpers::read_account_from_disc(&value.project_name, pubkey))
            .collect::<Result<Vec<AccountSchema>>>()
            .unwrap_or_default();

        let accounts = value.accounts.iter()
            .map(|(pubkey, _)| helpers::read_account_from_disc(&value.project_name, pubkey))
            .collect::<Result<Vec<AccountSchema>>>()
            .unwrap_or_default();
        
        Self { 
            project_name: value.project_name,
            networks: value.networks,
            programs: programs,
            accounts: accounts,
            idls: value.idls,
            overrides: value.overrides,
            compose: value.compose,
        }
    }
}

impl Valid8Context {

    pub fn init(name: Option<String>) -> Result<Valid8Context>{
        let mut project_name = ProjectName::default();
        if let Some(name) =  name {
            project_name = ProjectName::from_str(&name)?;
        }
        
        if let Ok((config, installed)) = Self::try_open_config(&project_name) {
            if !installed {
                let items = vec!["Install"];

                let selection = Select::new()
                    .with_prompt("Accounts not yet installed, select install, or press ESC to exit?")
                    .items(&items)
                    .interact_opt()?;

                if let Some(n) = selection {
                    match n {
                        0 => {Ok(
                            config.to_context()?
                        )},
                        _ => Err(anyhow!("Invalid option. Exit.")),
                    }
                } else {
                    Err(anyhow!("Accounts not installed. Exit"))
                }

            } else {
                println!("{} config found, accounts installed: {}", project_name.to_config(), installed);
                Ok(config.into())
            }
        } else {
            Self::try_init_config(&project_name)
        }
    }

    pub fn create_resources_dir(project_name: &ProjectName) -> Result<()> {
        create_dir_all(Path::new(&project_name.to_resources()))?;
        Ok(())
    }

    pub fn create_project_config(project_name: &ProjectName) -> Result<File> {
        let file = File::create(Path::new(&project_name.to_config()))?;
        Ok(file)
    }

    pub fn try_init_config(project_name: &ProjectName) -> Result<Self> {
        let mut ctx = Valid8Context::default();
        ctx.project_name = project_name.clone();
        let pretty_string = serde_json::to_string_pretty(&ConfigJson::from(ctx.clone()))?;

        // Create resources dir for this project
        Self::create_resources_dir(&project_name)
            // Create config json for project
            .and_then(|_| Self::create_project_config(&project_name))
            // Write config to file
            .and_then(|mut file| Ok(file.write_all(pretty_string.as_bytes())))??;

        Ok(ctx)
    }

    pub fn try_save_config(&self) -> Result<()> {

        let pretty_string = serde_json::to_string_pretty(&ConfigJson::from(self.clone()))?;
        File::create(Path::new(&self.project_name.to_config()))
            .and_then(|mut file|file.write_all(pretty_string.as_bytes()))?;
        Ok(())
    }

    pub fn try_open_config(project_name: &ProjectName) -> Result<(ConfigJson, bool)> {
        let mut buf = vec![];
        File::open(Path::new(&project_name.to_config()))
            .and_then(|mut file| file.read_to_end(&mut buf))?;
        let config: ConfigJson = serde_json::from_slice(&buf)?;
        println!("Config {:?}", &config);
    
        // Convert ConfigJson to Valid8Context, this also tries to read accounts from disc
        let mut installed = true;
        if !&config.is_installed() {
            println!("Accounts not found in local workspace, please run valid8 install to clone them.");
            installed = false;
        }

        Ok((config, installed))
    }

    pub fn try_compose(&self) -> Result<u8> {

        let mut this_ctx: ConfigJson = self.clone().into();
        let mut compose_count = 0;
        let mut new_config_path = self.compose.clone();

        while let Some(new_config) = new_config_path.clone() {
            compose_count += 1;

            if compose_count>20{return Err(anyhow!(compose_count))};
            
            let (new_ctx, _) = Valid8Context::try_open_config(&ProjectName::from_str(&new_config.replace(".json", ""))?)?;

            new_ctx.accounts.iter().for_each(|new_acc| {
                if !this_ctx.accounts.contains(new_acc) {this_ctx.accounts.push(new_acc.clone())}
            });
    
            new_ctx.programs.iter().for_each(|new_prog| {
                if !this_ctx.programs.contains(new_prog) {this_ctx.programs.push(new_prog.clone())}
            });
    
            new_ctx.idls.iter().for_each(|new_idl| {
                if !this_ctx.idls.contains(new_idl) {this_ctx.idls.push(new_idl.clone())}
            });
    
            new_ctx.networks.iter().for_each(|new_network| {
                this_ctx.networks.insert(new_network.clone());
            });
    
            if let Some(new_overrides) = new_ctx.overrides {
                new_overrides.iter().for_each(|new_over| {
                    if let Some(overrides) = this_ctx.overrides.as_mut(){
                        if !overrides.contains(new_over) {overrides.push(new_over.clone())}
                    }
                });
            }
            new_config_path = new_ctx.compose;
        }
        let new_context = this_ctx.to_context()?;
        new_context.try_save_config()?;

        Ok(compose_count)
    }

    pub fn has_account(&self, pubkey: &Pubkey) -> bool {
        self.accounts.iter().find(|acc| acc.pubkey == *pubkey).is_some() 
    }

    pub fn has_program(&self, program_id: &Pubkey) -> bool {
        self.programs.iter().find(|acc| acc.pubkey == *program_id).is_some()
    }

    pub fn add_program(&mut self, network: &Network, program_id: &Pubkey) -> Result<()> {
        // Check if we have the program in our hashmap already
        if self.has_program(&program_id) {
            println!("{} already added", &program_id.to_string());
            return Ok(())
        }
        self.add_program_unchecked(network, program_id)
    }

    pub fn add_program_unchecked(&mut self, network: &Network, program_id: &Pubkey) -> Result<()> {
        // Get program account
        let program_account = helpers::fetch_account(&network, &program_id)?;

        match program_id.to_string().as_ref() {
            "BPFLoaderUpgradeab1e11111111111111111111111" => {  },
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" => {  },
            "11111111111111111111111111111111" => {  },
            _address => {
                self.programs.push(program_account.clone());

                // Clone program data
                let program_data_account = helpers::clone_program_data(self, &program_account)?;
                self.accounts.push(program_data_account);
            
                // Get IDL address
                if let Ok(_) = helpers::clone_idl(&self, &program_account) {
                    self.add_idl(&program_id)?
                }
            }
        }
        // Save program account

        self.try_save_config()
    }

    pub fn add_account(&mut self, network: &Network, pubkey: &Pubkey) -> Result<()> {
        // Check if we have the account in our accounts
        if self.has_account(&pubkey) {
            println!("{} already added", &pubkey.to_string());
            return Ok(())
        }
        self.add_account_unchecked(network, pubkey)
    }

    pub fn add_account_unchecked(&mut self, network: &Network, pubkey: &Pubkey) -> Result<()> {
        // Get account
        let account = helpers::fetch_account(&network, &pubkey)?;

        // Save program account
        self.accounts.push(account.clone());
        self.networks.insert(network.clone());

        match self.has_program(&account.owner) {
            true => self.try_save_config(),
            false => self.add_program_unchecked(&network, &account.owner)
        }
        
    }

    pub fn add_idl(&mut self, program_id: &Pubkey) -> Result<()> {


        self.idls.push(program_id.to_string());
        Ok(())
    }

    pub fn get_account(&mut self, pubkey: &Pubkey,) -> Result<AccountSchema> {
        let position = self.accounts
            .iter()
            .position(|acc| acc.pubkey == *pubkey)
            .ok_or(anyhow!("No account found in context; Edit"))?;
        
        Ok(self.accounts.remove(position))
    }

    pub fn add_override(&mut self, over: Override){
        if let Some(override_list) = self.overrides.as_mut() {
            override_list.push(over)
        } else {
            self.overrides = Some(vec![over])
        }
    }

    pub fn apply_overrides(&mut self) -> Result<()> {
        if let Some (override_list) = self.overrides.clone() {
            let _ = override_list.iter().map(|over| {
                if self.accounts.iter().find(|acc| acc.pubkey == over.pubkey).is_some() {
                    let _ = over.edit_fields
                        .iter()
                        .map(|edit_field| self.edit_account(&over.pubkey, edit_field.clone()))
                        .collect::<Result<Vec<()>>>();
                } else if self.programs.iter().find(|acc| acc.pubkey == over.pubkey).is_some() { 
                    let _ = over.edit_fields
                        .iter()
                        .map(|edit_field| self.edit_program(&over.pubkey, edit_field.clone()))
                        .collect::<Result<Vec<()>>>();
                } else {
                    println!("Account not found in context!: {}", over.pubkey);
                }
            });
        }
        Ok(())
    }

    pub fn edit_account(&mut self, pubkey: &Pubkey, edit_field: EditField) -> Result<()> {
       
        let mut account = self.get_account(pubkey)?;

        match edit_field {
            EditField::Lamports(new_lamports) => {
                account.lamports = new_lamports
            }
            EditField::Owner(new_owner) => {
                account.owner = new_owner
            },
            EditField::UpgradeAuthority(_new_pubkey) => return Err(anyhow!("No upgrade authoprity on account")),
            EditField::Data(_) => todo!(),
        }

        helpers::save_account_to_disc(&self.project_name, &account)?;
        self.accounts.push(account);
        self.add_override(Override::new(*pubkey, edit_field));
        self.try_save_config()?;
        
        Ok(())
    }

    pub fn edit_program(&mut self, pubkey: &Pubkey, edit_field: EditField) -> Result<()> {

        let mut program_data_account = self.get_account(pubkey)?;

        match &edit_field {
            EditField::Lamports(new_lamports) => {
                program_data_account.lamports = *new_lamports
            }
            EditField::Owner(new_owner) => {
                program_data_account.owner = *new_owner
            },
            EditField::UpgradeAuthority(new_upgrade_auth) => {
                let new_statue = UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address: Some(*new_upgrade_auth),
                };
                let mut acc = program_data_account.to_account()?;
                acc.set_state(&new_statue)?;
                program_data_account = AccountSchema::from_account(&acc, &program_data_account.pubkey, &program_data_account.network)?;
            },
            EditField::Data(_json_value) => {
                
            },

        }
        helpers::save_account_to_disc(&self.project_name, &program_data_account)?;
        self.accounts.push(program_data_account);
        self.add_override(Override::new(*pubkey, edit_field));
        self.try_save_config()?;

        Ok(())
    }

    // pub fn create_ledger(&self) -> Result<()> {

    //     let mut config = TestValidatorGenesis::default();
    //     config.ledger_path(&self.project_name.to_ledger_path());
    //     println!("self {:?}", &self);
    //     println!("ledger path {}", &self.project_name.to_string());
    //     for program in &self.programs {
    //         let acc = AccountSharedData::from(program.to_account()?);
    //         println!("prog {:#?}", acc);
    //         config.add_account(program.pubkey, AccountSharedData::from(program.to_account()?));
    //     }
    //     for account in &self.accounts {
    //         let acc = AccountSharedData::from(account.to_account()?);
            
    //         println!("acc {:#?}", acc);
            
    //         config.add_account(account.pubkey, AccountSharedData::from(account.to_account()?));
    //     }

    //     // config.
    //     let (test_validator, _tv_keypair) = config.start();
    //     println!("{:?}", test_validator.cluster_info().all_peers());
    //     std::thread::sleep(std::time::Duration::from_secs(3));
    //     drop(test_validator);
    //     println!("Custom ledger created");


    //     Ok(())
    // }

    pub fn create_ledger(&self) -> Result<()> {

        // // for start, mimic the testvalidator genesis config and ledger with the necessary keys
        let mint_address = Keypair::new();
        let validator_identity = Keypair::new();
        let validator_vote_account = Keypair::new();
        let validator_stake_account = Keypair::new();
        let validator_identity_lamports = sol_to_lamports(500.);
        let validator_stake_lamports = sol_to_lamports(1_000_000.);
        let mint_lamports = sol_to_lamports(500_000_000.);


        // let (mut genesis_config, keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        
        let mut accounts: HashMap<Pubkey, AccountSharedData> = HashMap::new();

        for program in &self.programs {
            accounts.insert(program.pubkey, AccountSharedData::from(program.to_account()?));
            // genesis_config.add_account(program.pubkey, AccountSharedData::from(program.to_account()?));
        }
        
        for account in &self.accounts {
            accounts.insert(account.pubkey, AccountSharedData::from(account.to_account()?));
            // genesis_config.add_account(account.pubkey, AccountSharedData::from(account.to_account()?));
        }

        let mut genesis_config = create_genesis_config_with_leader_ex(
            mint_lamports,
            &mint_address.pubkey(),
            &validator_identity.pubkey(),
            &validator_vote_account.pubkey(),
            &validator_stake_account.pubkey(),
            validator_stake_lamports,
            validator_identity_lamports,
            FeeRateGovernor::default(),
            Rent::default(),
            solana_sdk::genesis_config::ClusterType::Development,
            accounts.into_iter().collect(),
        );

        // let mut genesis_config_info = create_genesis_config_with_leader(
        //     mint_lamports,
        //     // &mint_address.pubkey(),
        //     &validator_identity.pubkey(),
        //     // &validator_vote_account.pubkey(),
        //     // &validator_stake_account.pubkey(),
        //     validator_stake_lamports,
        //     // validator_identity_lamports,
        //     // FeeRateGovernor::default(),
        //     // Rent::default(),
        //     // solana_sdk::genesis_config::ClusterType::Development,
        //     // accounts.into_iter().collect(),
        // );
        genesis_config.epoch_schedule = EpochSchedule::without_warmup();

        // println!("{:#?}", genesis_config);
        let test_ledger_path = Path::new("test-ledger");

        let _last_hash = create_new_ledger(
            // Path::new(&self.project_name.to_ledger_path()),
            test_ledger_path,
            &genesis_config,
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            LedgerColumnOptions::default(),
        )
        .map_err(|err| {
            anyhow!(
                "Failed to create ledger at {}: {}",
                self.project_name.to_ledger_path(),
                err
            )
        })?;
        // let project_ledger = &self.project_name.to_ledger_path();

        write_keypair_file(
            &validator_identity,
            test_ledger_path.join("validator-keypair.json").to_str().unwrap(),
        ).unwrap();

        write_keypair_file(
            &validator_stake_account,
            test_ledger_path
                .join("stake-account-keypair.json")
                .to_str()
                .unwrap(),
        ).unwrap();

        write_keypair_file(
            &validator_vote_account,
            test_ledger_path
                .join("vote-account-keypair.json")
                .to_str()
                .unwrap(),
        ).unwrap();
        // println!("ledger created: {}", self.project_name.to_ledger_path());
        println!("ledger directory created: test-ledger");

        Ok(())
    }
}