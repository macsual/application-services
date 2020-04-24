/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub use crate::http_client::GetAttachedClientResponse as AttachedClient;
use crate::{error::*, util, CachedResponse, FirefoxAccount};

// An attached client response is considered fresh for `ATTACHED_CLIENTS_FRESHNESS_THRESHOLD` ms.
const ATTACHED_CLIENTS_FRESHNESS_THRESHOLD: u64 = 60_000; // 1 minute

impl FirefoxAccount {
    pub fn get_attached_clients(&mut self) -> Result<Vec<AttachedClient>> {
        if let Some(a) = &self.attached_clients {
            if util::now() < a.cached_at + ATTACHED_CLIENTS_FRESHNESS_THRESHOLD {
                return Ok(a.response.clone());
            }
        }
        let refresh_token = self.get_refresh_token()?;
        let response = self
            .client
            .attached_clients(&self.state.config, &refresh_token)?;
        let attached_clients = response.response;

        self.attached_clients = Some(CachedResponse {
            response: attached_clients.clone(),
            cached_at: util::now(),
            etag: response.etag.unwrap_or_default(),
        });

        Ok(attached_clients)
    }
}
