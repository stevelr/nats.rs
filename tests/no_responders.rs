// Copyright 2020-2022 The NATS Authors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod util;
pub use util::*;

#[tokio::test]
async fn no_responders() {
    let s = util::run_basic_server();
    let nc = nats_aflowt::connect(&s.client_url())
        .await
        .expect("could not connect");
    let res = nc.request("nobody-home", "hello").await;
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert!(err.to_string().contains("no responders"), "{}", err);
}
