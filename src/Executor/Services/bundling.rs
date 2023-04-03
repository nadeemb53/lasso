use config::Config;
use ethers::providers::{JsonRpcProvider, Provider};
use logger::Logger;
use mempool::MempoolService;
use reputation::ReputationService;
use std::sync::Mutex;
use user_op_validation::UserOpValidationService;

struct BundlingService {
    mutex: Mutex<()>,
    bundling_mode: BundlingMode,
    auto_bundling_interval: u64,
    auto_bundling_cron: Option<std::thread::JoinHandle<()>>,
    max_mempool_size: u64,
    network: NetworkName,
    provider: JsonRpcProvider,
    mempool_service: MempoolService,
    user_op_validation_service: UserOpValidationService,
    reputation_service: ReputationService,
    config: Config,
    logger: Logger,
}

impl BundlingService {
    fn new(
        network: NetworkName,
        provider: JsonRpcProvider,
        mempool_service: MempoolService,
        user_op_validation_service: UserOpValidationService,
        reputation_service: ReputationService,
        config: Config,
        logger: Logger,
    ) -> BundlingService {
        BundlingService {
            mutex: Mutex::new(()),
            bundling_mode: BundlingMode::Auto,
            auto_bundling_interval: 15 * 1000,
            auto_bundling_cron: None,
            max_mempool_size: 2,
            network,
            provider,
            mempool_service,
            user_op_validation_service,
            reputation_service,
            config,
            logger,
        }
    }

    async fn send_next_bundle(&self) -> Option<SendBundleReturn> {
        match self.mutex.lock() {
            Ok(_guard) => {
                self.logger.debug("sendNextBundle");
                let bundle = self.create_bundle().await;
                if bundle.is_empty() {
                    self.logger.debug("sendNextBundle - no bundle");
                    None
                } else {
                    self.send_bundle(bundle).await
                }
            }
            Err(_) => None,
        }
    }

    async fn send_bundle(
        &self,
        bundle: Vec<MempoolEntry>,
    ) -> Result<Option<SendBundleReturn>, Box<dyn std::error::Error>> {
        if bundle.is_empty() {
            return Ok(None);
        }
        let entry_point = bundle[0].entry_point;
        let entry_point_contract = EntryPoint::factory().connect(entry_point, &self.provider)?;
        let wallet = self
            .config
            .get_relayer(self.network)
            .ok_or("no relayer found")?;
        let beneficiary = self.select_beneficiary().await?;
        let tx_request = entry_point_contract.interface.encode_function_data(
            "handleOps",
            &[
                bundle
                    .iter()
                    .map(|entry| entry.user_op.clone())
                    .collect::<Vec<UserOp>>(),
                beneficiary,
            ],
        );
        let tx = wallet.send_transaction(
            TransactionRequest::new()
                .to(entry_point)
                .data(Bytes::from(tx_request))
                .finalize()
                .await?,
        )?;

        self.logger
            .debug(format!("Sent new bundle {}", tx.hash().unwrap()));

        for entry in bundle {
            self.mempool_service.remove(&entry).await?;
        }

        let user_op_hashes = self
            .get_user_op_hashes(&entry_point_contract, &bundle)
            .await?;
        self.logger
            .debug(format!("User op hashes {:?}", user_op_hashes));
        Ok(Some(SendBundleReturn {
            transaction_hash: tx.hash().unwrap(),
            user_op_hashes,
        }))
    }

    async fn create_bundle(&mut self) -> Result<Vec<MempoolEntry>> {
        // TODO: support multiple entry points
        //       filter bundles by entry points
        let entries = self.mempool_service.get_sorted_ops().await?;
        let mut bundle = vec![];

        let mut paymaster_deposit = HashMap::new();
        let mut staked_entity_count = HashMap::new();
        let mut senders = HashSet::new();

        for entry in entries {
            let paymaster = get_addr(entry.user_op.paymaster_and_data);
            let factory = get_addr(entry.user_op.init_code);

            if let Some(paymaster_addr) = paymaster {
                let paymaster_status = self.reputation_service.get_status(paymaster_addr).await?;
                if paymaster_status == ReputationStatus::Banned {
                    self.mempool_service.remove(&entry).await?;
                    continue;
                } else if paymaster_status == ReputationStatus::Throttled
                    || staked_entity_count
                        .get(&paymaster_addr)
                        .cloned()
                        .unwrap_or(0)
                        > 1
                {
                    self.logger.debug(
                        "skipping throttled paymaster",
                        object! {
                            metadata: {
                                sender: entry.user_op.sender,
                                nonce: entry.user_op.nonce,
                                paymaster,
                            },
                        },
                    );
                    continue;
                }
            }

            if let Some(factory_addr) = factory {
                let deployer_status = self.reputation_service.get_status(factory_addr).await?;
                if deployer_status == ReputationStatus::Banned {
                    self.mempool_service.remove(&entry).await?;
                    continue;
                } else if deployer_status == ReputationStatus::Throttled
                    || staked_entity_count.get(&factory_addr).cloned().unwrap_or(0) > 1
                {
                    self.logger.debug(
                        "skipping throttled factory",
                        object! {
                            metadata: {
                                sender: entry.user_op.sender,
                                nonce: entry.user_op.nonce,
                                factory,
                            },
                        },
                    );
                    continue;
                }
            }

            if senders.contains(&entry.user_op.sender) {
                self.logger.debug(
                    "skipping already included sender",
                    object! {
                        metadata: {
                            sender: entry.user_op.sender,
                            nonce: entry.user_op.nonce,
                        },
                    },
                );
                continue;
            }

            let validation_result = match self
                .user_op_validation_service
                .simulate_complete_validation(&entry.user_op, &entry.entry_point, entry.hash)
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    self.logger.debug(format!("failed 2nd validation: {}", e));
                    self.mempool_service.remove(&entry).await?;
                    continue;
                }
            };

            let entry_point_contract =
                EntryPoint::factory().connect(entry.entry_point.clone(), self.provider.clone());
            if let Some(paymaster_addr) = paymaster {
                if !paymaster_deposit.contains_key(&paymaster_addr) {
                    paymaster_deposit.insert(
                        paymaster_addr.clone(),
                        entry_point_contract
                            .balance_of(paymaster_addr.clone())
                            .await?,
                    );
                }
                // if let Some(paymaster_balance) = paymaster_deposit.get_mut(&paymaster_addr) {
                //     if paymaster_balance < &validation_result.return_info.prefund {
                //         // not enough balance in paymaster to pay for all UserOps
                //         // (but it passed validation, so it can sponsor them separately)
                //         continue;
                //     }
                //     *staked_entity_count.entry(pay
            }
        }
    }
}
