// Copyright (c) 2020, Fraunhofer AISEC. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{FiniteStateMachine, FsmEvent};
use crate::drivers::rat_driver::{RatDriver, RatMessage, RatRegistry};

use openssl::x509::X509;

use std::marker::PhantomData;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::thread;
use thiserror::Error;

///////////// Rat Driver Types for Generic Implementation ////////////////
pub(super) struct RatProver;
pub(super) struct RatVerifier;

pub(super) trait RatDriverType {
    fn create_event(msg: RatMessage) -> FsmEvent;
}

impl RatDriverType for RatProver {
    fn create_event(msg: RatMessage) -> FsmEvent {
        FsmEvent::FromRatProver(msg)
    }
}

impl RatDriverType for RatVerifier {
    fn create_event(msg: RatMessage) -> FsmEvent {
        FsmEvent::FromRatVerifier(msg)
    }
}
/////////////////////////////////////////////////////////////////////////

#[derive(Error, Debug)]
pub enum RatError {
    #[error("Cannot access RAT registry")]
    RegistryNotAvailable,
    #[error("RAT driver is not available in the RAT registry")]
    UnknownRatDriver,
    #[error("No RAT driver activated")]
    RatDriverInactive,
    #[error("Connection to the RAT driver was aborted")]
    RatConnectionAborted,
    #[error("RAT driver has not been cached")]
    RatDriverNotCached,
}

// Rat Driver Interfaces
// A rat driver interface is owned by the FiniteStateMachine and can only be used by this FSM
// Since a FSM is only accessible via mutex lock, the RatDriverInterface can only be accessed by
// one thread at a time
struct RatDriverContent {
    tx_to_driver: Sender<RatMessage>,
    tx_to_listener: Sender<RatMessage>,
    listener: DriverListener,
}

pub(super) struct RatDriverInterface<RatType: RatDriverType + Send + Sync + 'static> {
    pub(super) fsm: Weak<Mutex<FiniteStateMachine>>,
    content: Option<RatDriverContent>,
    cached_driver: Option<Arc<dyn RatDriver + Send + Sync>>,
    phantom: PhantomData<RatType>,
    peer_cert: X509,
}

impl<RatType: RatDriverType + Send + Sync + 'static> RatDriverInterface<RatType> {
    pub(super) fn create(peer_cert: X509) -> Arc<Mutex<RatDriverInterface<RatType>>> {
        Arc::new(Mutex::new(RatDriverInterface {
            fsm: Weak::new(),
            content: None,
            cached_driver: None,
            phantom: PhantomData,
            peer_cert,
        }))
    }

    pub(super) fn start_driver(
        &mut self,
        rat_mechanism: &str,
        registry: Weak<RatRegistry>,
        strong_ref_interface: Arc<Mutex<RatDriverInterface<RatType>>>,
    ) -> Result<(), RatError> {
        // terminate running driver
        self.stop_driver();

        // get driver from registry
        let registry = match registry.upgrade() {
            None => {
                return Err(RatError::RegistryNotAvailable);
            }

            Some(r) => r,
        };

        let driver_clone = match registry.get_driver(rat_mechanism) {
            None => {
                return Err(RatError::UnknownRatDriver);
            }
            Some(driver) => Arc::clone(driver),
        };

        //cache driver clone
        self.cached_driver = Some(driver_clone);
        self.run_driver(strong_ref_interface)
    }

    pub(super) fn restart_driver(
        &mut self,
        strong_ref_interface: Arc<Mutex<RatDriverInterface<RatType>>>,
    ) -> Result<(), RatError> {
        // terminate running driver
        self.stop_driver();

        self.run_driver(strong_ref_interface)
    }

    fn run_driver(
        &mut self,
        strong_ref_interface: Arc<Mutex<RatDriverInterface<RatType>>>,
    ) -> Result<(), RatError> {
        let driver_clone = match &self.cached_driver {
            None => {
                return Err(RatError::RatDriverNotCached);
            }

            Some(driver) => Arc::clone(driver),
        };

        // create channels
        let (tx_to_driver, rx_from_interface) = mpsc::channel();
        let (tx_to_interface, rx_from_driver) = mpsc::channel();
        let p1 = self.peer_cert.clone();

        // create listener
        let listener = DriverListener::new();

        // create interface content
        let content = RatDriverContent {
            tx_to_driver,
            tx_to_listener: tx_to_interface.clone(),
            listener,
        };
        self.content = Some(content);

        // execute driver
        thread::spawn(move || {
            driver_clone.execute(tx_to_interface, rx_from_interface, p1);
        });

        //start listener
        let fsm_clone = Weak::clone(&self.fsm);
        let content = self.content.as_mut().unwrap();
        content
            .listener
            .listen::<RatType>(fsm_clone, strong_ref_interface, rx_from_driver);

        Ok(())
    }

    pub(super) fn write_to_driver(&self, msg: RatMessage) -> Result<(), RatError> {
        let sender = match &self.content {
            None => return Err(RatError::RatDriverInactive),

            Some(content) => &content.tx_to_driver,
        };

        match sender.send(msg) {
            Err(_) => Err(RatError::RatConnectionAborted),
            Ok(_) => Ok(()),
        }
    }

    pub(super) fn stop_driver(&mut self) {
        //take content from interface is available and than terminate listener and close channels
        log::debug!("decoupling from RAT driver");
        match self.content.take() {
            None => log::debug!("not connected to a RAT driver. Nothing to do."),

            Some(mut content) => {
                //stop listener
                content.listener.stop();

                //send something to listener to unblock receive
                let _ = content.tx_to_listener.send(RatMessage::RawData(vec![]));

                //close channels
                drop(content.tx_to_listener);
                drop(content.tx_to_driver);
            }
        };
    }

    fn on_driver_stop(&mut self) {
        let content = match self.content.take() {
            None => {
                return;
            }
            Some(content) => content,
        };

        drop(content.tx_to_driver);
        drop(content.tx_to_listener);
    }
}

struct DriverListener {
    is_cancelled: Arc<Mutex<bool>>,
    is_locked: bool,
}

impl DriverListener {
    fn new() -> DriverListener {
        DriverListener {
            is_cancelled: Arc::new(Mutex::new(false)),
            is_locked: false,
        }
    }

    //from upper layer, start listener thread
    fn listen<RatType: RatDriverType + Send + Sync + 'static>(
        &mut self,
        fsm: Weak<Mutex<FiniteStateMachine>>,
        interface: Arc<Mutex<RatDriverInterface<RatType>>>,
        rx_from_driver: Receiver<RatMessage>,
    ) {
        if self.is_locked {
            log::warn!("Driver Listener was already in use, but can only be started once");
            return;
        }
        //set in use
        self.is_locked = true;

        //create clones for listener thread
        let is_cancelled_clone = Arc::clone(&self.is_cancelled);

        //enter cancel lock
        let mut cancel_guard = self.is_cancelled.lock().unwrap();

        //set cancel lock to false
        (*cancel_guard) = false;

        //spawn thread
        thread::spawn(move || {
            let driver_stop_handler = || {
                let mut interface_guard = interface.lock().unwrap();
                (*interface_guard).on_driver_stop();
            };

            loop {
                match rx_from_driver.recv() {
                    Err(_) => {
                        // driver closed, notify interface and terminate listener
                        driver_stop_handler();
                        return;
                    }
                    Ok(msg) => {
                        //received new message

                        //lock fsm
                        let fsm_strong = match fsm.upgrade() {
                            None => {
                                log::debug!("FSM is not available anymore");
                                driver_stop_handler();
                                return;
                            }
                            Some(strong) => strong,
                        };
                        let mut fsm_guard = match fsm_strong.lock() {
                            Err(_) => {
                                log::error!("FSM lock failed");

                                // notify interface
                                driver_stop_handler();
                                return;
                            }
                            Ok(guard) => guard,
                        };

                        //check if cancelled
                        let cancel_guard = is_cancelled_clone.lock().unwrap();
                        let cancelled = *cancel_guard;
                        if cancelled {
                            log::debug!("Driver listener has been cancelled");
                            return;
                        } else {
                            // driver was not cancelled, delegate message to fsm
                            let _ = (*fsm_guard).process_event(RatType::create_event(msg));
                        }
                    }
                }
            }
        });
    }

    //from upper layer, terminate listener
    fn stop(&mut self) {
        //enter cancel lock
        let mut cancel_guard = self.is_cancelled.lock().unwrap();

        //set cancel lock to true
        (*cancel_guard) = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::idscp_configuration::AttestationConfig;
    use crate::drivers::daps_driver::DapsDriver;
    use crate::drivers::rat_driver;
    use crate::drivers::secure_channel::SecureChannel;
    use crate::fsm::HandshakeResult;
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509Name, X509};
    use std::io::Error;
    use std::sync::Condvar;
    use std::time::Duration;

    struct RatDummy {}
    impl rat_driver::RatDriver for RatDummy {
        fn get_id(&self) -> &'static str {
            "RatDummy"
        }

        fn execute(&self, _tx: Sender<RatMessage>, rx: Receiver<RatMessage>, _peer_cert: X509) {
            println!("Dummy has been started");
            match rx.recv() {
                Err(_) => {
                    println!("Quit dummy, channel was closed");
                    return;
                }

                Ok(_) => {
                    println!("Dummy received msg");
                }
            }

            match rx.recv() {
                Err(_) => {
                    println!("Quit dummy, channel was closed");
                    return;
                }

                Ok(_) => {
                    println!("Dummy received msg");
                }
            }
        }
    }

    struct TestDaps {}
    impl DapsDriver for TestDaps {
        fn get_token(&self) -> String {
            "valid".to_string()
        }

        fn verify_token(&self, token: &String) -> Option<Duration> {
            if token.eq("valid") {
                Some(Duration::from_millis(1000))
            } else {
                None
            }
        }
    }

    struct TestSc {}

    impl SecureChannel for TestSc {
        fn send_msg(&self, _data: Vec<u8>) -> Result<(), Error> {
            Ok(())
        }

        fn recv_msg(&self) -> Result<Vec<u8>, Error> {
            Ok(vec![])
        }

        fn terminate(&self) {}

        fn get_peer_certificate(&self) -> X509 {
            let rsa = Rsa::generate(2048).unwrap();
            let pkey = PKey::from_rsa(rsa).unwrap();

            let mut name = X509Name::builder().unwrap();
            name.append_entry_by_nid(Nid::COMMONNAME, "foobar.com")
                .unwrap();
            let name = name.build();

            let mut builder = X509::builder().unwrap();
            builder.set_version(2).unwrap();
            builder.set_subject_name(&name).unwrap();
            builder.set_issuer_name(&name).unwrap();
            builder.set_pubkey(&pkey).unwrap();
            builder.sign(&pkey, MessageDigest::sha256()).unwrap();

            let certificate: X509 = builder.build();
            return certificate;
        }
    }

    #[test]
    fn rat_interface_test() {
        //create registry
        let verifier_registry = RatRegistry::new();
        let mut prover_registry = RatRegistry::new();
        let prover = RatDummy {};
        prover_registry.register_driver(Arc::new(prover));

        let handshake_cond = Arc::new((Mutex::new(HandshakeResult::NotAvailable), Condvar::new()));
        //create fsm
        let fsm = FiniteStateMachine::create(
            Arc::new(TestSc {}),
            prover_registry,
            verifier_registry,
            Arc::new(TestDaps {}),
            handshake_cond,
            Duration::from_millis(5000),
            Duration::from_millis(1000),
            AttestationConfig {
                supported_attestation_suite: vec![],
                expected_attestation_suite: vec![],
                rat_timeout: Duration::from_millis(1000),
            },
        );

        //get fsm lock
        let fsm_guard = fsm.lock().unwrap();

        //get prover interface clones
        let prover_arc = Arc::clone(&fsm_guard.rat_prover);

        let prover_strong = Arc::clone(&prover_arc);

        //get prover lock
        let mut prover_guard = prover_arc.lock().unwrap();

        //check if content is none
        assert!((*prover_guard).content.is_none());

        let mut prover_registry = RatRegistry::new();
        let prover = RatDummy {};
        prover_registry.register_driver(Arc::new(prover));
        let prover_registry = Arc::new(prover_registry);

        //start prover driver

        assert!((*prover_guard)
            .start_driver("RatDummy", Arc::downgrade(&prover_registry), prover_strong)
            .is_ok());

        //check if content is some
        assert!((*prover_guard).content.is_some());

        //write to prover
        assert!((*prover_guard)
            .write_to_driver(RatMessage::RawData(Vec::from("Hello")))
            .is_ok());

        //stop prover again
        (*prover_guard).stop_driver();

        //check if content is none
        assert!((*prover_guard).content.is_none());
    }
}
