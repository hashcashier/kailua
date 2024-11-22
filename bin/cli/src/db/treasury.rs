// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloy::network::Network;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use anyhow::Context;
use kailua_contracts::KailuaTreasury::KailuaTreasuryInstance;
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Treasury {
    pub index: u64,
    pub address: Address,
    pub elimination_round: HashMap<Address, u64>,
    pub claim_proposer: HashMap<Address, Address>,
    pub participation_bond: U256,
    pub paid_bond: HashMap<Address, U256>,
}

impl Treasury {
    pub async fn init<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        treasury_instance: &KailuaTreasuryInstance<T, P, N>,
    ) -> anyhow::Result<Self> {
        // Load participation bond
        let participation_bond = treasury_instance
            .participationBond()
            .call()
            .await
            .context("participation_bond")?
            ._0;
        let index = treasury_instance
            .gameIndex()
            .call()
            .await
            .context("game_index")?
            ._0
            .to();
        Ok(Self {
            index,
            address: *treasury_instance.address(),
            elimination_round: Default::default(),
            claim_proposer: Default::default(),
            participation_bond,
            paid_bond: Default::default(),
        })
    }

    pub fn treasury_contract_instance<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        &self,
        provider: P,
    ) -> KailuaTreasuryInstance<T, P, N> {
        KailuaTreasuryInstance::new(self.address, provider)
    }

    pub async fn fetch_bond<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        &mut self,
        provider: P,
    ) -> anyhow::Result<U256> {
        self.participation_bond = self
            .treasury_contract_instance(provider)
            .participationBond()
            .call()
            .await
            .context("participation_bond")?
            ._0;
        Ok(self.participation_bond)
    }

    pub async fn fetch_balance<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        &mut self,
        provider: P,
        address: Address,
    ) -> anyhow::Result<U256> {
        let paid_bond = self
            .treasury_contract_instance(provider)
            .paidBonds(address)
            .call()
            .await
            .context("paid_bonds")?
            ._0;
        self.paid_bond.insert(address, paid_bond);
        Ok(paid_bond)
    }

    pub async fn fetch_proposer<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        &mut self,
        provider: P,
        address: Address,
    ) -> anyhow::Result<Address> {
        if !self.claim_proposer.contains_key(&address) {
            let proposer = self
                .treasury_contract_instance(provider)
                .proposer(address)
                .call()
                .await
                .context("proposer")?
                ._0;
            self.claim_proposer.insert(address, proposer);
        }
        Ok(*self.claim_proposer.get(&address).unwrap())
    }

    pub async fn fetch_elimination_round<T: Transport + Clone, P: Provider<T, N>, N: Network>(
        &mut self,
        provider: P,
        address: Address,
    ) -> anyhow::Result<u64> {
        if !self.elimination_round.contains_key(&address) {
            let round = self
                .treasury_contract_instance(provider)
                .eliminationRound(address)
                .call()
                .await
                .context("proposer")?
                ._0
                .to();
            self.elimination_round.insert(address, round);
        }
        Ok(*self.elimination_round.get(&address).unwrap())
    }
}