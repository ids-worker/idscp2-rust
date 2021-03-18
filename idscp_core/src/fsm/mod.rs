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

pub(super) mod alternating_bit;
mod fsm_timer;
mod rat_interface;
mod sc_interface;

use crate::api::idscp_configuration::AttestationConfig;
use crate::api::idscp_connection::InnerIdscp2connection;
use crate::drivers::daps_driver::DapsDriver;
use crate::drivers::rat_driver::{RatIcm, RatMessage, RatRegistry};
use crate::drivers::secure_channel::SecureChannel;
use crate::messages::idscp_message_factory;
use crate::messages::idscpv2_messages::*;
use fsm_timer::*;

use crate::fsm::rat_interface::RatError;
use crate::fsm::sc_interface::ScIfError;
use protobuf::Message;
use rat_interface::{RatDriverInterface, RatProver, RatVerifier};
use sc_interface::SecureChannelInterface;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;
use thiserror::Error;

use crate::fsm::alternating_bit::AlternatingBitError;
use alternating_bit::AlternatingBit;

// FSM Events
#[derive(Debug, Clone)]
enum FsmEvent {
    // RAT DRIVER EVENTS
    FromRatProver(RatMessage),
    FromRatVerifier(RatMessage),

    // USER EVENTS
    FromUpper(UserEvent),

    // SECURE CHANNEL EVENTS
    FromSecureChannel(SecureChannelEvent),

    // TIMEOUT EVENTS
    RatTimeout,
    DatTimeout,
    HandshakeTimeout,
    AckTimeout,
}

#[derive(Debug, Clone)]
enum SecureChannelEvent {
    Close(IdscpClose),
    Hello(IdscpHello),
    Dat(IdscpDat),
    DatExp(IdscpDatExpired),
    RatProver(IdscpRatProver),
    RatVerifier(IdscpRatVerifier),
    ReRat(IdscpReRat),
    Data(IdscpData),
    Error,
    Ack(IdscpAck),
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    StartHandshake,
    Stop,
    RepeatRat,
    Data(Vec<u8>),
}

#[derive(Debug, PartialEq, Clone)]
enum ClosedStateStatus {
    Locked,
    Unlocked,
}

// FSM States
#[derive(Debug, PartialEq, Clone)]
enum FsmState {
    Closed(ClosedStateStatus), //nothing active
    WaitForHello,              //handshake active
    WaitForRat,                //prover + verifier active
    WaitForRatProver,          //prover active
    WaitForRatVerifier,        //verifier active
    WaitForDatAndRat,          //handshake + prover active
    WaitForDatAndRatVerifier,  //handshake active
    WaitForAck,                //AckTimeout active
    Established,               //nothing active
}

//idscp2 handshake result
#[derive(Debug, PartialEq)]
pub enum HandshakeResult {
    NotAvailable,
    Failed,
    Successful,
}

// AckFlag
#[derive(Clone, Debug, PartialEq)]
pub enum AckFlag {
    Active(Vec<u8>),
    Inactive,
}

#[derive(Error, Debug, PartialEq)]
pub enum RatNegotiationError {
    #[error("No RAT mechanism match found")]
    NoRatMechanismMatch,
}

#[derive(Error, Debug)]
pub enum FsmError {
    #[error("No transition available for the given event")]
    UnknownTransition,
    #[error("FSM is locked forever")]
    FsmLocked,
    #[error("FSM handshake was never started")]
    FsmNotStarted,
    #[error("DAT is missing")]
    MissingDat,
    #[error("DAT is invalid")]
    InvalidDat,
    #[error("Cannot send or receive message via the secure channel interface")]
    IoError(#[from] ScIfError),
    #[error("RAT action failed")]
    RatError(#[from] RatError),
    #[error("Error during negotiation of RAT mechanisms")]
    RatNegotiationError(#[from] RatNegotiationError),
    #[error("Operation would block until FSM is in state 'Established'")]
    WouldBlock,
    #[error(
        "Action failed because FSM was started but is currently not connected. Try it later again"
    )]
    NotConnected,
    #[error("IdscpData must be buffered in state 'WaitForAck'")]
    IdscpDataNotCached,
}

// FSM
pub(crate) struct FiniteStateMachine {
    rat_prover: Arc<Mutex<RatDriverInterface<RatProver>>>,
    rat_verifier: Arc<Mutex<RatDriverInterface<RatVerifier>>>,
    current_state: FsmState,
    handshake_timer: StaticTimer<HandshakeTimer>,
    prover_timer: StaticTimer<HandshakeTimer>, // TODO: maybe make new timer type "RatDriverTimer" to emit more precise error?
    verifier_timer: StaticTimer<HandshakeTimer>, // TODO: maybe make new timer type "RatDriverTimer" to emit more precise error?
    rat_timer: StaticTimer<RatTimer>,
    ack_timer: StaticTimer<AckTimer>,
    dat_timer: DynamicTimer<DatTimer>,
    sc_interface: Arc<Mutex<SecureChannelInterface>>,
    daps_driver: Arc<dyn DapsDriver + Send + Sync>,
    prover_registry: Arc<RatRegistry>,
    verifier_registry: Arc<RatRegistry>,
    idscp_connection: Weak<Mutex<InnerIdscp2connection>>,
    connection_available_var: Arc<(Mutex<bool>, Condvar)>, //wait until connection is available
    handshake_cond: Arc<(Mutex<HandshakeResult>, Condvar)>, //handshake result to notify upper layer
    handshake_result_available: bool,
    rat_config: AttestationConfig,
    ack_flag: AckFlag,
    expected_alternating_bit: AlternatingBit,
    next_send_alternating_bit: AlternatingBit,
}

impl FiniteStateMachine {
    pub fn create(
        secure_channel: Arc<dyn SecureChannel + Send + Sync>,
        prover_registry: RatRegistry,
        verifier_registry: RatRegistry,
        daps_driver: Arc<dyn DapsDriver + Send + Sync>,
        handshake_cond: Arc<(Mutex<HandshakeResult>, Condvar)>,
        handshake_timeout: Duration,
        ack_timeout: Duration,
        rat_config: AttestationConfig,
    ) -> Arc<Mutex<FiniteStateMachine>> {
        let peer_cert = secure_channel.get_peer_certificate();
        let prover: Arc<Mutex<RatDriverInterface<RatProver>>> =
            RatDriverInterface::create(peer_cert.clone());
        let verifier: Arc<Mutex<RatDriverInterface<RatVerifier>>> =
            RatDriverInterface::create(peer_cert);
        let sc_interface = SecureChannelInterface::create();

        //create fsm in arc mutex for multi-threaded mutable access
        let fsm = Arc::new(Mutex::new(FiniteStateMachine {
            rat_prover: Arc::clone(&prover),
            rat_verifier: Arc::clone(&verifier),
            current_state: FsmState::Closed(ClosedStateStatus::Unlocked),
            handshake_timer: StaticTimer::new(handshake_timeout),
            prover_timer: StaticTimer::new(handshake_timeout),
            verifier_timer: StaticTimer::new(handshake_timeout),
            rat_timer: StaticTimer::new(rat_config.rat_timeout),
            dat_timer: DynamicTimer::new(),
            sc_interface: Arc::clone(&sc_interface),
            daps_driver,
            prover_registry: Arc::new(prover_registry),
            verifier_registry: Arc::new(verifier_registry),
            idscp_connection: Weak::new(),
            connection_available_var: Arc::new((Mutex::new(false), Condvar::new())),
            handshake_cond,
            handshake_result_available: false,
            rat_config,
            ack_flag: AckFlag::Inactive,
            ack_timer: StaticTimer::new(ack_timeout),
            expected_alternating_bit: AlternatingBit::new(),
            next_send_alternating_bit: AlternatingBit::new(),
        }));

        prover.lock().unwrap().fsm = Arc::downgrade(&fsm);
        verifier.lock().unwrap().fsm = Arc::downgrade(&fsm);
        {
            let mut sc_interface_guard = sc_interface.lock().unwrap();
            sc_interface_guard.fsm = Arc::downgrade(&fsm);
            sc_interface_guard.init(secure_channel, Arc::downgrade(&sc_interface));
        }

        {
            let mut guard = fsm.lock().unwrap();
            (*guard).handshake_timer.set_fsm(Arc::downgrade(&fsm));
            (*guard).prover_timer.set_fsm(Arc::downgrade(&fsm));
            (*guard).verifier_timer.set_fsm(Arc::downgrade(&fsm));
            (*guard).dat_timer.set_fsm(Arc::downgrade(&fsm));
            (*guard).rat_timer.set_fsm(Arc::downgrade(&fsm));
            (*guard).ack_timer.set_fsm(Arc::downgrade(&fsm));
        }
        fsm
    }

    pub fn set_connection(&mut self, connection: Option<Weak<Mutex<InnerIdscp2connection>>>) {
        //set connection if available
        match connection {
            Some(c) => self.idscp_connection = c,
            None => {}
        }

        //set connection available variable
        let &(ref lock, ref cvar) = &*self.connection_available_var;
        match lock.lock() {
            Err(e) => {
                log::error!("Cannot acquire connection_available lock: {}", e);
            }

            Ok(mut guard) => {
                (*guard) = true;
            }
        }
        cvar.notify_all();
    }

    pub fn feed_user_event(&mut self, e: UserEvent) -> Result<(), FsmError> {
        let event = FsmEvent::FromUpper(e);
        self.process_event(event)
    }

    pub fn is_closed(&self) -> bool {
        match self.current_state {
            FsmState::Closed(_) => true,
            _ => false,
        }
    }

    pub fn is_connected(&self) -> bool {
        match self.current_state {
            FsmState::Established => true,
            FsmState::WaitForAck => true,
            _ => false,
        }
    }

    fn process_event(&mut self, event: FsmEvent) -> Result<(), FsmError> {
        log::info!(
            "FSM triggered by event{:?} in state {:?}",
            event,
            self.current_state
        );

        use ClosedStateStatus::*;
        use FsmEvent::*;
        use FsmState::*;

        let mut res: Result<(), FsmError> = Ok(());

        match &self.current_state {
            Closed(status) => {
                //check internal Closed state
                match status {
                    Locked => {
                        //ignore all events
                        log::warn!("FSM is locked forever");
                        res = Err(FsmError::FsmLocked);
                    }

                    Unlocked => match event {
                        FromUpper(UserEvent::StartHandshake) => {
                            match self.action_start_handshake() {
                                Err(e) => {
                                    log::warn!("Error occurred during starting handshake: {}", e);
                                    self.cleanup();
                                    self.notify_connection_about_close(); // inspected: no deadlock, asynchronous notification to the connection
                                    self.current_state =
                                        FsmState::Closed(ClosedStateStatus::Locked);
                                    res = Err(e);
                                }

                                Ok(_) => {
                                    self.handshake_timer.start();
                                    self.current_state = FsmState::WaitForHello;
                                }
                            }
                        }

                        FromUpper(UserEvent::RepeatRat)
                        | FromUpper(UserEvent::Data(_))
                        | FromUpper(UserEvent::Stop) => {
                            log::warn!(
                                "User action not available since FSM handshake was never started"
                            );
                            res = Err(FsmError::FsmNotStarted);
                        }

                        _ => {
                            log::warn!("No transition available, stay in state Closed");
                            res = Err(FsmError::UnknownTransition);
                        }
                    },
                }
            }

            WaitForHello => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                FromUpper(UserEvent::RepeatRat) => {
                    //nothing to do, res should be OK(()) since Rat will be done in the next state
                    // for the first time
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred in secure channel. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Hello(data) => match self.action_recv_hello(data) {
                        Err(e) => {
                            log::error!("Cannot handle IdscpHello");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForRat;
                        }
                    },

                    _ => {
                        log::warn!(
                            "No transition available for {:?}, stay in state WaitForHello",
                            sc_event
                        );
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForHello");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForRat => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                FromUpper(UserEvent::RepeatRat) => {
                    //nothing to do, res should be OK(()) since Rat will be done in the next state
                    // for the first time
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                DatTimeout => match self.dat_timeout_handler() {
                    Err(e) => {
                        log::warn!("Error occurred during handling dat timeout: {}", e);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        res = Err(e);
                    }
                    Ok(_) => {
                        self.handshake_timer.start();
                        self.current_state = WaitForDatAndRat;
                    }
                },

                FromRatProver(msg) => match msg {
                    RatMessage::ControlMessage(RatIcm::OK) => {
                        log::debug!("Received RatProverOK");
                        self.prover_timer.cancel();
                        self.current_state = WaitForRatVerifier;
                    }

                    RatMessage::ControlMessage(RatIcm::Failed) => {
                        self.action_rat_prover_failed();
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    RatMessage::RawData(data) => match self.action_rat_prover_data(data) {
                        Err(e) => {
                            log::warn!("Cannot send RatProver msg");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        _ => {}
                    },
                },

                FromRatVerifier(msg) => match msg {
                    RatMessage::ControlMessage(RatIcm::OK) => {
                        log::debug!("Received RatVerifierOk");
                        self.verifier_timer.cancel();
                        self.rat_timer.start();
                        self.current_state = WaitForRatProver;
                    }

                    RatMessage::ControlMessage(RatIcm::Failed) => {
                        self.action_rat_verifier_failed();
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    RatMessage::RawData(data) => match self.action_rat_verifier_data(data) {
                        Err(e) => {
                            log::warn!("Cannot send RatVerifier msg");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {}
                    },
                },

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                        Err(e) => {
                            log::warn!("Error occurred during DatExpired handling {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = WaitForRat;
                        }
                    },

                    SecureChannelEvent::RatProver(data) => {
                        if let Err(e) = self.action_delegate_rat_prover(data) {
                            log::warn!("Cannot delegate RatProver msg to RatVerifier: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                    }

                    SecureChannelEvent::RatVerifier(data) => {
                        if let Err(e) = self.action_delegate_rat_verifier(data) {
                            log::warn!("Cannot delegate RatVerifier msg to RatProver: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                    }

                    SecureChannelEvent::Ack(ack_data) => {
                        match self.action_recv_ack(ack_data) {
                            Ok(_) => {
                                // no state change
                            }
                            Err(err) => {
                                log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                // no state change
                            }
                        }
                    }

                    _ => {
                        log::warn!("No transition available, stay in state WaitForRat");
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForRat");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForRatProver => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::RepeatRat) | RatTimeout => match self.action_re_rat() {
                    Err(e) => {
                        log::warn!("Error occurred during re_rat handling: {}", e);
                        self.cleanup();
                        self.notify_connection_about_close(); // inspected: no deadlock, asynchronous notification to the connection
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        res = Err(e);
                    }

                    Ok(_) => self.current_state = FsmState::WaitForRat,
                },

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                DatTimeout => match self.dat_timeout_handler() {
                    Err(e) => {
                        log::warn!("Error occurred during handling dat timeout: {}", e);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        res = Err(e);
                    }
                    Ok(_) => {
                        self.handshake_timer.start();
                        self.current_state = WaitForDatAndRat;
                    }
                },

                FromRatProver(msg) => match msg {
                    RatMessage::ControlMessage(RatIcm::OK) => {
                        log::debug!("Received RatProverOK");
                        self.prover_timer.cancel();
                        self.current_state = match self.ack_flag {
                            AckFlag::Inactive => Established,
                            AckFlag::Active(_) => {
                                self.ack_timer.start();
                                WaitForAck
                            }
                        };
                    }

                    RatMessage::ControlMessage(RatIcm::Failed) => {
                        self.action_rat_prover_failed();
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    RatMessage::RawData(data) => match self.action_rat_prover_data(data) {
                        Err(e) => {
                            log::warn!("Cannot send RatProver msg");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        _ => {}
                    },
                },

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                        Err(e) => {
                            log::warn!("Error occurred during DatExpired handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = WaitForRatProver;
                        }
                    },

                    SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during receiving re_rat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForRatProver;
                        }
                    },

                    SecureChannelEvent::RatVerifier(data) => {
                        if let Err(e) = self.action_delegate_rat_verifier(data) {
                            log::warn!("Cannot delegate RatVerifier msg to RatProver: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                    }

                    SecureChannelEvent::Ack(ack_data) => {
                        match self.action_recv_ack(ack_data) {
                            Ok(_) => {
                                // no state change
                            }
                            Err(err) => {
                                log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                // no state change
                            }
                        }
                    }

                    _ => {
                        log::warn!("No transition available, stay in state WaitForRatProver");
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForRatProver");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForRatVerifier => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                FromUpper(UserEvent::RepeatRat) => {
                    //nothing to do, res should be OK(()) since Rat will be done in the next state
                    // for the first time
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                DatTimeout => match self.dat_timeout_handler() {
                    Err(e) => {
                        log::warn!("Error occurred during handling dat timeout: {}", e);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        res = Err(e);
                    }
                    Ok(_) => {
                        self.handshake_timer.start();
                        self.current_state = WaitForDatAndRatVerifier;
                    }
                },

                FromRatVerifier(msg) => match msg {
                    RatMessage::ControlMessage(RatIcm::OK) => {
                        log::debug!("Received RatVerifierOk");
                        self.verifier_timer.cancel();
                        self.rat_timer.start();
                        self.current_state = match self.ack_flag {
                            AckFlag::Inactive => Established,
                            AckFlag::Active(_) => {
                                self.ack_timer.start();
                                WaitForAck
                            }
                        };
                    }

                    RatMessage::ControlMessage(RatIcm::Failed) => {
                        self.action_rat_verifier_failed();
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    RatMessage::RawData(data) => match self.action_rat_verifier_data(data) {
                        Err(e) => {
                            log::warn!("Cannot send RatVerifier msg");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {}
                    },
                },

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                        Err(e) => {
                            log::warn!("Error occurred during DatExpired handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = WaitForRat;
                        }
                    },

                    SecureChannelEvent::RatProver(data) => {
                        if let Err(e) = self.action_delegate_rat_prover(data) {
                            log::warn!("Cannot delegate RatProver msg to RatVerifier: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                    }

                    SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during receiving re_rat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForRat;
                        }
                    },

                    SecureChannelEvent::Ack(ack_data) => {
                        match self.action_recv_ack(ack_data) {
                            Ok(_) => {
                                // no state change
                            }
                            Err(err) => {
                                log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                // no state change
                            }
                        }
                    }

                    _ => {
                        log::warn!("No transition available, stay in state WaitForRatVerifier");
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForRatVerifier");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForDatAndRat => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                FromUpper(UserEvent::RepeatRat) => {
                    //nothing to do, res should be OK(()) since Rat will be done in the next state
                    // for the first time
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromRatProver(msg) => match msg {
                    RatMessage::ControlMessage(RatIcm::OK) => {
                        log::debug!("Received RatProverOK");
                        self.prover_timer.cancel();
                        self.current_state = WaitForDatAndRatVerifier;
                    }

                    RatMessage::ControlMessage(RatIcm::Failed) => {
                        self.action_rat_prover_failed();
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    RatMessage::RawData(data) => match self.action_rat_prover_data(data) {
                        Err(e) => {
                            log::warn!("Cannot send RatProver msg");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        _ => {}
                    },
                },

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                        Err(e) => {
                            log::warn!("Error occurred during DatExpired handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = WaitForDatAndRat;
                        }
                    },

                    SecureChannelEvent::Dat(data) => match self.action_recv_dat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during validating dat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForRat;
                        }
                    },

                    SecureChannelEvent::RatVerifier(data) => {
                        if let Err(e) = self.action_delegate_rat_verifier(data) {
                            log::warn!("Cannot delegate RatVerifier msg to RatProver: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                    }

                    SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during receiving re_rat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForDatAndRat;
                        }
                    },

                    SecureChannelEvent::Ack(ack_data) => {
                        match self.action_recv_ack(ack_data) {
                            Ok(_) => {
                                // no state change
                            }
                            Err(err) => {
                                log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                // no state change
                            }
                        }
                    }

                    _ => {
                        log::warn!("No transition available, stay in state WaitForDatAndRat");
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForDatAndRat");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForDatAndRatVerifier => match event {
                FromUpper(UserEvent::Stop) => {
                    self.action_stop();
                    self.cleanup();
                    // no need to notify idscp_connection. it caused closing itself.
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromUpper(UserEvent::Data(_)) => {
                    res = Err(FsmError::NotConnected);
                }

                FromUpper(UserEvent::RepeatRat) => {
                    //nothing to do, res should be OK(()) since Rat will be done in the next state
                    // for the first time
                }

                HandshakeTimeout => {
                    self.handshake_timeout_handler();
                    self.cleanup();
                    self.notify_connection_about_close();
                    self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                }

                FromSecureChannel(sc_event) => match sc_event {
                    SecureChannelEvent::Error => {
                        log::debug!("Error occurred. Close Idscp2 connection");
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::Close(close) => {
                        log::debug!("Received IdscpClose: {}", close.cause_msg);
                        self.cleanup();
                        self.notify_connection_about_close();
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                        Err(e) => {
                            log::warn!("Error occurred during DatExpired handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = WaitForDatAndRat;
                        }
                    },

                    SecureChannelEvent::Dat(data) => match self.action_recv_dat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during validating dat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForRatVerifier;
                        }
                    },

                    SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                        Err(e) => {
                            log::warn!("Error occurred during receiving re_rat: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.current_state = FsmState::WaitForDatAndRat;
                        }
                    },

                    SecureChannelEvent::Ack(ack_data) => {
                        match self.action_recv_ack(ack_data) {
                            Ok(_) => {
                                // no state change
                            }
                            Err(err) => {
                                log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                // no state change
                            }
                        }
                    }

                    _ => {
                        log::warn!(
                            "No transition available, stay in state WaitForDatAndRatVerifier"
                        );
                        res = Err(FsmError::UnknownTransition);
                    }
                },

                _ => {
                    log::warn!("No transition available, stay in state WaitForDatAndRatVerifier");
                    res = Err(FsmError::UnknownTransition);
                }
            },

            WaitForAck => {
                match event {
                    // user events
                    FromUpper(UserEvent::Stop) => {
                        self.action_stop();
                        self.cleanup();
                        // no need to notify idscp_connection. it caused closing itself.
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    FromUpper(UserEvent::RepeatRat) | RatTimeout => match self.action_re_rat() {
                        Err(e) => {
                            log::warn!("Error occurred during re_rat handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close(); // inspected: no deadlock, asynchronous notification to the connection
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }

                        Ok(_) => {
                            self.ack_timer.cancel();
                            self.current_state = FsmState::WaitForRatVerifier
                        }
                    },

                    FromUpper(UserEvent::Data(_)) => {
                        log::warn!("Cannot send data in WaitForAck state");
                        res = Err(FsmError::WouldBlock);
                    }

                    // timeouts
                    DatTimeout => match self.dat_timeout_handler() {
                        Err(e) => {
                            log::warn!("Error occurred during handling dat timeout: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.ack_timer.cancel();
                            self.handshake_timer.start();
                            self.current_state = WaitForDatAndRatVerifier;
                        }
                    },

                    AckTimeout => match self.ack_flag.clone() {
                        AckFlag::Inactive => {
                            log::error!("No IdscpData message buffered in state 'WaitForAck'");
                            res = Err(FsmError::IdscpDataNotCached)
                        }
                        AckFlag::Active(data) => match self.action_send_data(data) {
                            Err(e) => {
                                log::warn!("Error occurred during sending data");
                                self.cleanup();
                                self.notify_connection_about_close();
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.ack_timer.start();
                            }
                        },
                    },

                    FromSecureChannel(sc_event) => match sc_event {
                        SecureChannelEvent::Error => {
                            log::debug!("Error occurred. Close Idscp2 connection");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        }

                        SecureChannelEvent::Close(close) => {
                            log::debug!("Received IdscpClose: {}", close.cause_msg);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        }

                        SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                            Err(e) => {
                                log::warn!("Error occurred during DatExpired handling: {}", e);
                                self.cleanup();
                                self.notify_connection_about_close();
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.ack_timer.cancel();
                                self.current_state = WaitForRatProver;
                            }
                        },

                        SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                            Err(e) => {
                                log::warn!("Error occurred during receiving re_rat: {}", e);
                                self.cleanup();
                                self.notify_connection_about_close();
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.ack_timer.cancel();
                                self.current_state = FsmState::WaitForRatProver;
                            }
                        },

                        SecureChannelEvent::Data(data) => self.action_recv_data(data),

                        SecureChannelEvent::Ack(ack_data) => {
                            match self.action_recv_ack(ack_data) {
                                Ok(_) => self.current_state = FsmState::Established,
                                Err(err) => {
                                    log::debug!("Ignoring received IdscpAck due to: {:?}", err)
                                    // no state change
                                }
                            }
                        }

                        _ => {
                            log::warn!("No transition available, stay in state WaitForAck");
                            res = Err(FsmError::UnknownTransition);
                        }
                    },

                    _ => {
                        log::warn!("No transition available, stay in state WaitForAck");
                        res = Err(FsmError::UnknownTransition);
                    }
                }
            }

            Established => {
                match event {
                    // user events
                    FromUpper(UserEvent::Stop) => {
                        self.action_stop();
                        self.cleanup();
                        // no need to notify idscp_connection. it caused closing itself.
                        self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                    }

                    FromUpper(UserEvent::RepeatRat) | RatTimeout => match self.action_re_rat() {
                        Err(e) => {
                            log::warn!("Error occurred during re_rat handling: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close(); // inspected: no deadlock, asynchronous notification to the connection
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }

                        Ok(_) => self.current_state = FsmState::WaitForRatVerifier,
                    },

                    FromUpper(UserEvent::Data(msg)) => {
                        match self.action_send_data(msg.clone()) {
                            Err(e) => {
                                log::warn!("Error occurred during sending data");
                                self.cleanup();
                                self.notify_connection_about_close(); // inspected: no deadlock, asynchronous notification to the connection
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.ack_flag = AckFlag::Active(msg);
                                self.ack_timer.start();
                                self.current_state = FsmState::WaitForAck;
                            }
                        }
                    }

                    // timeouts
                    DatTimeout => match self.dat_timeout_handler() {
                        Err(e) => {
                            log::warn!("Error occurred during handling dat timeout: {}", e);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                            res = Err(e);
                        }
                        Ok(_) => {
                            self.handshake_timer.start();
                            self.current_state = WaitForDatAndRatVerifier;
                        }
                    },

                    FromSecureChannel(sc_event) => match sc_event {
                        SecureChannelEvent::Error => {
                            log::debug!("Error occurred. Close Idscp2 connection");
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        }

                        SecureChannelEvent::Close(close) => {
                            log::debug!("Received IdscpClose: {}", close.cause_msg);
                            self.cleanup();
                            self.notify_connection_about_close();
                            self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                        }

                        SecureChannelEvent::DatExp(_) => match self.action_recv_dat_exp() {
                            Err(e) => {
                                log::warn!("Error occurred during DatExpired handling: {}", e);
                                self.cleanup();
                                self.notify_connection_about_close();
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.current_state = WaitForRatProver;
                            }
                        },

                        SecureChannelEvent::ReRat(data) => match self.action_recv_re_rat(data) {
                            Err(e) => {
                                log::warn!("Error occurred during receiving re_rat: {}", e);
                                self.cleanup();
                                self.notify_connection_about_close();
                                self.current_state = FsmState::Closed(ClosedStateStatus::Locked);
                                res = Err(e);
                            }
                            Ok(_) => {
                                self.current_state = FsmState::WaitForRatProver;
                            }
                        },

                        SecureChannelEvent::Data(data) => self.action_recv_data(data),

                        _ => {
                            log::warn!("No transition available, stay in state Established");
                            res = Err(FsmError::UnknownTransition);
                        }
                    },

                    _ => {
                        log::warn!("No transition available, stay in state Established");
                        res = Err(FsmError::UnknownTransition);
                    }
                }
            }
        };

        //set handshake result
        let handshake_cond_clone = Arc::clone(&self.handshake_cond);
        let set_handshake_result = move |result: HandshakeResult| {
            let &(ref lock, ref cvar) = &*handshake_cond_clone;
            match lock.lock() {
                Err(e) => {
                    log::error!("Cannot acquire handshake lock: {}", e);
                }

                Ok(mut handshake_result) => {
                    *handshake_result = result;
                }
            }
            cvar.notify_all();
        };

        if !self.handshake_result_available {
            match self.current_state {
                FsmState::Established => {
                    // handshake successful
                    (set_handshake_result)(HandshakeResult::Successful);
                    self.handshake_result_available = true;
                }

                FsmState::Closed(ClosedStateStatus::Locked) => {
                    // handshake failed
                    (set_handshake_result)(HandshakeResult::Failed);
                    self.handshake_result_available = true;
                }

                _ => {}
            }
        }

        log::info!(
            "After processing event, FSM is in state: {:?}",
            self.current_state
        );

        //return result
        res
    } //end of process_event

    fn action_start_handshake(&mut self) -> Result<(), FsmError> {
        log::debug!("Starting IDSCP2 Handshake ...");

        //unlock secure channel listener
        let _ = self.sc_interface.lock().unwrap().unlock();

        //get dat from daps
        let dat = self.daps_driver.get_token();

        //create idscp_hello msg
        let idscp_hello = idscp_message_factory::create_idscp_hello(
            dat.into_bytes(),
            &self.rat_config.expected_attestation_suite,
            &self.rat_config.supported_attestation_suite,
        );

        //send idscp hello via secure channel
        let mut data = Vec::new();
        let _ = idscp_hello.write_to_vec(&mut data);
        match self.sc_interface.lock().unwrap().write(data) {
            Err(e) => Err(FsmError::IoError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn dat_timeout_handler(&mut self) -> Result<(), FsmError> {
        log::debug!("Dat timeout occurred. Send IdscpDatExpired");
        self.rat_verifier.lock().unwrap().stop_driver();
        self.rat_timer.cancel();

        //send IdscpDatExpired
        let idscp_dat_exp = idscp_message_factory::create_idscp_dat_exp();
        let mut data = Vec::new();
        let _ = idscp_dat_exp.write_to_vec(&mut data);
        match self.sc_interface.lock().unwrap().write(data) {
            Err(e) => Err(FsmError::IoError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn handshake_timeout_handler(&mut self) {
        log::debug!("Handshake timeout occurred");

        //send close
        let idscp_close = idscp_message_factory::create_idscp_close(
            IdscpClose_CloseCause::TIMEOUT,
            "Handshake timeout",
        );
        let mut data = Vec::new();
        let _ = idscp_close.write_to_vec(&mut data);
        let _ = self.sc_interface.lock().unwrap().write(data);
    }

    fn action_stop(&mut self) {
        log::debug!("Close Idscp2 connection and send IdscpClose");

        //send close
        let idscp_close = idscp_message_factory::create_idscp_close(
            IdscpClose_CloseCause::USER_SHUTDOWN,
            "User shutdown",
        );

        let mut data = Vec::new();
        let _ = idscp_close.write_to_vec(&mut data);

        let sc = self.sc_interface.lock().unwrap();
        if let Err(e) = sc.write(data) {
            log::debug!("Cannot send IdscpClose: {}", e)
        }
    }

    fn calculate_rat_algorithms<'a>(
        primary: &'a [String],
        secondary: &'a [String],
    ) -> Result<&'a str, RatNegotiationError> {
        log::debug!("Calculate Rat mechanisms");
        for p in primary {
            for s in secondary {
                if p.eq(s) {
                    return Ok(p);
                }
            }
        }
        Err(RatNegotiationError::NoRatMechanismMatch)
    }

    fn calculate_rat_verifier_mechanism<'a>(
        peer_rat_supported_suites: &'a [String],
        own_rat_expected_suites: &'a [String],
    ) -> Result<&'a str, RatNegotiationError> {
        if peer_rat_supported_suites.is_empty() {
            log::error!("peer has no rat prover suites available");
            return Err(RatNegotiationError::NoRatMechanismMatch);
        }
        if own_rat_expected_suites.is_empty() {
            log::error!("own has no rat verifier suites available");
            return Err(RatNegotiationError::NoRatMechanismMatch);
        }
        FiniteStateMachine::calculate_rat_algorithms(
            own_rat_expected_suites,
            peer_rat_supported_suites,
        )
    }

    fn calculate_rat_prover_mechanism<'a>(
        peer_rat_expected_suites: &'a [String],
        own_rat_supported_suites: &'a [String],
    ) -> Result<&'a str, RatNegotiationError> {
        if peer_rat_expected_suites.is_empty() {
            log::error!("peer has no rat verifier suites available");
            return Err(RatNegotiationError::NoRatMechanismMatch);
        }
        if own_rat_supported_suites.is_empty() {
            log::error!("own has no rat prover suites available");
            return Err(RatNegotiationError::NoRatMechanismMatch);
        }
        FiniteStateMachine::calculate_rat_algorithms(
            peer_rat_expected_suites,
            own_rat_supported_suites,
        )
    }

    fn action_recv_hello(&mut self, hello: IdscpHello) -> Result<(), FsmError> {
        log::debug!("IdscpHello received");
        self.handshake_timer.cancel();

        let own_supported_provers = &self.rat_config.supported_attestation_suite;
        let peer_expected = hello.get_expectedRatSuite().to_vec();
        let prover_mechanism = FiniteStateMachine::calculate_rat_prover_mechanism(
            &peer_expected,
            &own_supported_provers,
        )?;

        let own_expected_verifiers = &self.rat_config.expected_attestation_suite;
        let peer_supported = hello.get_supportedRatSuite().to_vec();
        let verifier_mechanism = FiniteStateMachine::calculate_rat_verifier_mechanism(
            &peer_supported,
            &own_expected_verifiers,
        )?;

        let send_close = || {
            let idscp_close = idscp_message_factory::create_idscp_close(
                IdscpClose_CloseCause::NO_VALID_DAT,
                "No valid dat",
            );
            let mut data = Vec::new();
            let _ = idscp_close.write_to_vec(&mut data);
            let _ = self.sc_interface.lock().unwrap().write(data);
        };

        //get DAT from hello and verify DAT
        let remote_dat = match hello.dynamicAttributeToken.into_option() {
            None => {
                log::warn!("No dat available. Send Close and close connection");
                send_close();
                return Err(FsmError::MissingDat);
            }
            Some(dat) => match String::from_utf8(dat.token.to_vec()) {
                Err(_) => {
                    log::warn!("Cannot parse dat. Send close and close connection");
                    send_close();
                    return Err(FsmError::InvalidDat);
                }
                Ok(token) => token,
            },
        };

        match self.daps_driver.verify_token(&remote_dat) {
            None => {
                log::warn!("Dat is not valid. Send close and close connection");
                send_close();
                return Err(FsmError::InvalidDat);
            }

            Some(t) => {
                log::debug!("Dat is valid. Start dat timer");
                self.dat_timer.start(t);
            }
        }

        // start rat verifier
        log::debug!("Start rat prover and verifier");
        let mut verifier_guard = self.rat_verifier.lock().unwrap();
        if let Err(e) = (*verifier_guard).start_driver(
            &verifier_mechanism,
            Arc::downgrade(&self.verifier_registry),
            Arc::clone(&self.rat_verifier),
        ) {
            log::error!("Cannot start RatVerifier driver");
            return Err(FsmError::RatError(e));
        }
        self.verifier_timer.start();

        // start rat prover
        let mut prover_guard = self.rat_prover.lock().unwrap();
        if let Err(e) = (*prover_guard).start_driver(
            &prover_mechanism,
            Arc::downgrade(&self.prover_registry),
            Arc::clone(&self.rat_prover),
        ) {
            log::error!("Cannot start RatProver driver");
            return Err(FsmError::RatError(e));
        }
        self.prover_timer.start();

        Ok(())
    }

    fn action_send_data(&mut self, data: Vec<u8>) -> Result<(), FsmError> {
        let idscp_data =
            idscp_message_factory::create_idscp_data(data, &self.next_send_alternating_bit);
        let mut raw = Vec::new();
        let _ = idscp_data.write_to_vec(&mut raw);
        match self.sc_interface.lock().unwrap().write(raw) {
            Err(e) => Err(FsmError::IoError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn action_recv_data(&mut self, data: IdscpData) {
        log::debug!("Receive new message for connection (if connection available)");
        let recv_alternating_bit = AlternatingBit::from_bool(data.alternating_bit);
        if recv_alternating_bit != self.expected_alternating_bit {
            log::debug!("received IDSCPData with unexpected alternating bit. Could be an old packet replayed. Ignoring it.");
        } else {
            // send IdscpAck
            let idscp_ack = idscp_message_factory::create_idscp_ack(recv_alternating_bit);
            let mut raw = Vec::new();
            let _ = idscp_ack.write_to_vec(&mut raw);
            if self.sc_interface.lock().unwrap().write(raw).is_err() {
                log::error!("Cannot send IdscpAck");
            }
            self.expected_alternating_bit.alternate();

            // forward payload data to upper layer
            self.wait_for_connection_available();
            match self.idscp_connection.upgrade() {
                None => {
                    log::warn!("No connection available");
                }
                Some(c_lock) => {
                    log::debug!("try to aquire lock to connection");
                    let lock_result = c_lock.lock();
                    match lock_result {
                        Err(e) => {
                            log::warn!("Cannot acquire read lock: {}", e);
                        }
                        Ok(c_guard) => {
                            (*c_guard).on_message(Vec::from(data.get_data()));
                        }
                    }
                }
            }
        }
    }

    fn action_recv_ack(&mut self, ack_data: IdscpAck) -> Result<(), AlternatingBitError> {
        match self.ack_flag {
            AckFlag::Active(_) => {
                let acknoledged_alternating_bit =
                    AlternatingBit::from_bool(ack_data.alternating_bit);
                // compare with next_send_a_bit, which should be the copied into ack by peer
                if acknoledged_alternating_bit != self.next_send_alternating_bit {
                    Err(AlternatingBitError {})
                //Err(" with wrong alternating bit. Ignoring")
                } else {
                    log::debug!("Received valid IdscpAck, cancel ack_flag");
                    self.ack_flag = AckFlag::Inactive;
                    self.ack_timer.cancel();
                    // alternating bit correct, increase send bit for next message
                    self.next_send_alternating_bit.alternate();
                    Ok(())
                }
            }
            AckFlag::Inactive => Err(AlternatingBitError {}),
        }
    }

    fn action_re_rat(&mut self) -> Result<(), FsmError> {
        log::debug!("Repeat Rat. Send IdscpReRat and start RatVerifier");
        self.rat_timer.cancel();

        //send idscp re-rat
        let idscp_rerat = idscp_message_factory::create_idscp_re_rat("");
        let mut raw = Vec::new();
        let _ = idscp_rerat.write_to_vec(&mut raw);
        if let Err(e) = self.sc_interface.lock().unwrap().write(raw) {
            return Err(FsmError::IoError(e));
        }

        //start verifier
        let mut verifier_guard = self.rat_verifier.lock().unwrap();
        if let Err(e) = (*verifier_guard).restart_driver(Arc::clone(&self.rat_verifier)) {
            log::error!("Cannot restart RatVerifier driver");
            return Err(FsmError::RatError(e));
        }
        self.verifier_timer.start();
        Ok(())
    }

    fn action_recv_re_rat(&mut self, _data: IdscpReRat) -> Result<(), FsmError> {
        log::debug!(
            "Received IdscpReRat with cause: {}. Start RatProver",
            _data.cause
        );

        let mut prover_guard = self.rat_prover.lock().unwrap();
        if let Err(e) = (*prover_guard).restart_driver(Arc::clone(&self.rat_prover)) {
            log::error!("Cannot restart RatProver driver");
            return Err(FsmError::RatError(e));
        }
        self.prover_timer.start();

        Ok(())
    }

    fn action_rat_prover_failed(&mut self) {
        log::debug!("Received RatProver Failed");

        self.prover_timer.cancel();

        //send IdscpClose
        let idscp_close = idscp_message_factory::create_idscp_close(
            IdscpClose_CloseCause::RAT_PROVER_FAILED,
            "RatProver failed",
        );
        let mut data = Vec::new();
        let _ = idscp_close.write_to_vec(&mut data);
        let _ = self.sc_interface.lock().unwrap().write(data);
    }

    fn action_rat_prover_data(&mut self, data: Vec<u8>) -> Result<(), FsmError> {
        log::debug!("Send IdscpRatProver");

        let idscp_prover = idscp_message_factory::create_idscp_rat_prover(data);
        let mut raw = Vec::new();
        let _ = idscp_prover.write_to_vec(&mut raw);
        match self.sc_interface.lock().unwrap().write(raw) {
            Err(e) => Err(FsmError::IoError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn action_rat_verifier_failed(&mut self) {
        log::debug!("Received RatVerifier Failed");

        self.verifier_timer.cancel();

        //send close
        let idscp_close = idscp_message_factory::create_idscp_close(
            IdscpClose_CloseCause::RAT_VERIFIER_FAILED,
            "RatVerifier failed",
        );
        let mut data = Vec::new();
        let _ = idscp_close.write_to_vec(&mut data);
        let _ = self.sc_interface.lock().unwrap().write(data);
    }

    fn action_rat_verifier_data(&mut self, data: Vec<u8>) -> Result<(), FsmError> {
        log::debug!("Send IdscpRatVerifier");

        let idscp_verifier = idscp_message_factory::create_idscp_rat_verifier(data);
        let mut raw = Vec::new();
        let _ = idscp_verifier.write_to_vec(&mut raw);
        match self.sc_interface.lock().unwrap().write(raw) {
            Err(e) => Err(FsmError::IoError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn action_delegate_rat_prover(&mut self, data: IdscpRatProver) -> Result<(), FsmError> {
        log::debug!("Delegate received RatProver msg to RatVerifier");
        let verifier_guard = self.rat_verifier.lock().unwrap();
        match (*verifier_guard).write_to_driver(RatMessage::RawData(data.data.to_vec())) {
            Err(e) => Err(FsmError::RatError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn action_delegate_rat_verifier(&mut self, data: IdscpRatVerifier) -> Result<(), FsmError> {
        log::debug!("Delegate received RatVerifier msg to RatProver");
        let prover_guard = self.rat_prover.lock().unwrap();
        match (*prover_guard).write_to_driver(RatMessage::RawData(data.data.to_vec())) {
            Err(e) => Err(FsmError::RatError(e)),
            Ok(_) => Ok(()),
        }
    }

    fn action_recv_dat(&mut self, data: IdscpDat) -> Result<(), FsmError> {
        log::debug!("Receive IdscpDat. Verifying Dat ...");
        self.handshake_timer.cancel();

        let send_close = || {
            let idscp_close = idscp_message_factory::create_idscp_close(
                IdscpClose_CloseCause::NO_VALID_DAT,
                "No valid dat",
            );
            let mut data = Vec::new();
            let _ = idscp_close.write_to_vec(&mut data);
            let _ = self.sc_interface.lock().unwrap().write(data);
        };

        //get DAT from hello and verify DAT
        let remote_dat = match String::from_utf8(data.token.to_vec()) {
            Err(_) => {
                log::warn!("Cannot parse dat. Send close and close connection");
                send_close();
                return Err(FsmError::InvalidDat);
            }
            Ok(token) => token,
        };

        match self.daps_driver.verify_token(&remote_dat) {
            None => {
                log::warn!("Dat is not valid. Send close and close connection");
                send_close();
                return Err(FsmError::InvalidDat);
            }

            Some(t) => {
                log::debug!("Dat is valid. Start dat timer");
                self.dat_timer.start(t);
            }
        }

        log::debug!("Start RatVerifier");
        let mut verifier_guard = self.rat_verifier.lock().unwrap();
        if let Err(e) = (*verifier_guard).restart_driver(Arc::clone(&self.rat_verifier)) {
            log::error!("Cannot restart RatVerifier driver");
            return Err(FsmError::RatError(e));
        }
        self.verifier_timer.start();

        Ok(())
    }

    fn action_recv_dat_exp(&mut self) -> Result<(), FsmError> {
        log::debug!("Receive IdscpDatExpired. Send new Dat and start RatProver");

        //send new Dat
        let dat = self.daps_driver.get_token();
        let idscp_dat = idscp_message_factory::create_idscp_dat(dat.into_bytes());
        let mut raw = Vec::new();
        let _ = idscp_dat.write_to_vec(&mut raw);
        if let Err(e) = self.sc_interface.lock().unwrap().write(raw) {
            log::error!("Cannot send IdscpDat");
            return Err(FsmError::IoError(e));
        }

        let mut prover_guard = self.rat_prover.lock().unwrap();
        if let Err(e) = (*prover_guard).restart_driver(Arc::clone(&self.rat_prover)) {
            log::error!("Cannot restart RatProver driver");
            return Err(FsmError::RatError(e));
        }
        self.prover_timer.start();

        Ok(())
    }

    fn cleanup(&mut self) {
        self.handshake_timer.cancel();
        self.dat_timer.cancel();
        self.rat_timer.cancel();
        self.verifier_timer.cancel();
        self.prover_timer.cancel();
        self.ack_timer.cancel();

        self.rat_prover.lock().unwrap().stop_driver();
        self.rat_verifier.lock().unwrap().stop_driver();

        //close secure channel
        {
            let mut guard = self.sc_interface.lock().unwrap();
            let _ = (*guard).unlock();
            (*guard).stop();
        }
    }

    fn notify_connection_about_close(&self) {
        // notify connection about closure

        // if the handshake result was not available, the handshake seems to have failed
        // in this case, a connection will never be available. To avoid deadlocks with the user
        // thread that wait for the handshake result, which will be set directly after this cleanup
        // routine, the wait_for_connection_available function must be skipped in the case when no
        // handshake result is available so far. Otherwise, the connection is promised to be set to
        // None (on handshake failure) or Some (on handshake success).
        if self.handshake_result_available {
            self.wait_for_connection_available();
            match self.idscp_connection.upgrade() {
                None => {}
                Some(c_lock) => {
                    log::debug!("try to acquire lock to connection");
                    let lock_result = c_lock.lock();
                    log::debug!("connection lock acquired");
                    match lock_result {
                        Err(e) => {
                            log::warn!("Cannot aquire connection as write lock {}", e);
                        }
                        Ok(mut c_guard) => {
                            (*c_guard).on_close();
                        }
                    }
                }
            }
        }
    }

    fn wait_for_connection_available(&self) {
        // wait until connection result is available to avoid race conditions
        let &(ref lock, ref cvar) = &*self.connection_available_var;
        match lock.lock() {
            Err(e) => {
                log::error!("Cannot acquire connection_available lock: {}", e);
            }

            Ok(mut available) => {
                while !*available {
                    available = match cvar.wait(available) {
                        Err(e) => {
                            log::error!("Waiting for connection available failed: {}", e);
                            return;
                        }
                        Ok(guard) => guard,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    // Test Transitions //
    use super::*;
    use crate::drivers::daps_driver::DapsDriver;
    use crate::drivers::rat_driver::RatDriver;
    use crate::fsm::AckFlag::Inactive;
    use crate::messages::idscp_message_factory::*;
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509Name, X509};
    use std::io::Error;
    use std::sync::mpsc::{Receiver, Sender};
    use FsmEvent::*;
    use FsmState::*;

    struct TestDaps {}
    impl DapsDriver for TestDaps {
        fn get_token(&self) -> String {
            "valid".to_string()
        }

        fn verify_token(&self, token: &String) -> Option<Duration> {
            if token.eq("valid") {
                Some(Duration::from_secs(1))
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
            Ok(Vec::new())
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

    struct RatProverDummy {}
    struct RatVerifierDummy {}

    impl RatDriver for RatProverDummy {
        fn get_id(&self) -> &'static str {
            "NullRat"
        }

        fn execute(&self, _tx: Sender<RatMessage>, rx: Receiver<RatMessage>, _peer_cert: X509) {
            let _ = rx.recv();
        }
    }

    impl RatDriver for RatVerifierDummy {
        fn get_id(&self) -> &'static str {
            "NullRat"
        }

        fn execute(&self, _tx: Sender<RatMessage>, rx: Receiver<RatMessage>, _peer_cert: X509) {
            let _ = rx.recv();
        }
    }

    fn create_test_fsm(
        state: FsmState,
        ack_flag: AckFlag,
        next_send_alternating_bit: AlternatingBit,
        expected_alternating_bit: AlternatingBit,
    ) -> Arc<Mutex<FiniteStateMachine>> {
        let mut prover_registry = RatRegistry::new();
        let mut verifier_registry = RatRegistry::new();
        let prover = Arc::new(RatProverDummy {});
        let verifier = Arc::new(RatVerifierDummy {});
        prover_registry.register_driver(prover);
        verifier_registry.register_driver(verifier);
        let sc = Arc::new(TestSc {});
        let daps = Arc::new(TestDaps {});
        let handshake_cond = Arc::new((Mutex::new(HandshakeResult::NotAvailable), Condvar::new()));
        let handshake_timeout = Duration::from_millis(5000);
        let ack_timeout = Duration::from_millis(1000);
        let rat_config = AttestationConfig {
            supported_attestation_suite: vec!["NullRat".to_string()],
            expected_attestation_suite: vec!["NullRat".to_string()],
            rat_timeout: Duration::from_millis(1000),
        };
        let fsm = FiniteStateMachine::create(
            sc,
            prover_registry,
            verifier_registry,
            daps,
            handshake_cond,
            handshake_timeout,
            ack_timeout,
            rat_config,
        );

        // register rat drivers in interface (this would be done via receiving hello in normal
        // handshake and enables restart methods on interfaces)
        let rat_p_interface = Arc::clone(&fsm.lock().unwrap().rat_prover);
        let rat_v_interface = Arc::clone(&fsm.lock().unwrap().rat_verifier);
        let rat_p_registry = Arc::downgrade(&fsm.lock().unwrap().prover_registry);
        let rat_v_registry = Arc::downgrade(&fsm.lock().unwrap().verifier_registry);

        let _ = fsm.lock().unwrap().rat_prover.lock().unwrap().start_driver(
            "NullRat",
            rat_p_registry,
            rat_p_interface,
        );
        let _ = fsm
            .lock()
            .unwrap()
            .rat_verifier
            .lock()
            .unwrap()
            .start_driver("NullRat", rat_v_registry, rat_v_interface);

        let mut guard = fsm.lock().unwrap();
        (*guard).set_connection(None); //set no connection to ensure process_event is not
                                       // blocking until connection is available
        (*guard).current_state = state;
        (*guard).ack_flag = ack_flag;
        (*guard).next_send_alternating_bit = next_send_alternating_bit;
        (*guard).expected_alternating_bit = expected_alternating_bit;
        drop(guard);

        fsm
    }

    fn check_transition(s1: FsmState, s2: FsmState, e: FsmEvent, ack: AckFlag) -> bool {
        let fsm = create_test_fsm(s1, ack, AlternatingBit::new(), AlternatingBit::new());
        let mut guard = fsm.lock().unwrap();
        let _ = (*guard).process_event(e);
        (*guard).current_state == s2
    }

    fn get_sc_event(m: IdscpMessage) -> FsmEvent {
        let mut v = Vec::new();
        let _ = m.write_to_vec(&mut v);

        let event = match m.message.unwrap() {
            IdscpMessage_oneof_message::idscpClose(data) => SecureChannelEvent::Close(data),

            IdscpMessage_oneof_message::idscpHello(data) => SecureChannelEvent::Hello(data),

            IdscpMessage_oneof_message::idscpDat(data) => SecureChannelEvent::Dat(data),

            IdscpMessage_oneof_message::idscpDatExpired(data) => SecureChannelEvent::DatExp(data),

            IdscpMessage_oneof_message::idscpRatProver(data) => SecureChannelEvent::RatProver(data),

            IdscpMessage_oneof_message::idscpRatVerifier(data) => {
                SecureChannelEvent::RatVerifier(data)
            }

            IdscpMessage_oneof_message::idscpReRat(data) => SecureChannelEvent::ReRat(data),

            IdscpMessage_oneof_message::idscpData(data) => SecureChannelEvent::Data(data),

            IdscpMessage_oneof_message::idscpAck(data) => SecureChannelEvent::Ack(data),
        };

        FromSecureChannel(event)
    }

    fn locked() -> FsmState {
        Closed(ClosedStateStatus::Locked)
    }

    fn unlocked() -> FsmState {
        Closed(ClosedStateStatus::Unlocked)
    }

    fn p_msg() -> FsmEvent {
        FromRatProver(RatMessage::RawData(vec![]))
    }

    fn p_ok() -> FsmEvent {
        FromRatProver(RatMessage::ControlMessage(RatIcm::OK))
    }

    fn p_failed() -> FsmEvent {
        FromRatProver(RatMessage::ControlMessage(RatIcm::Failed))
    }

    fn v_msg() -> FsmEvent {
        FromRatVerifier(RatMessage::RawData(vec![]))
    }

    fn v_ok() -> FsmEvent {
        FromRatVerifier(RatMessage::ControlMessage(RatIcm::OK))
    }

    fn v_failed() -> FsmEvent {
        FromRatVerifier(RatMessage::ControlMessage(RatIcm::Failed))
    }

    fn sc_err() -> FsmEvent {
        FromSecureChannel(SecureChannelEvent::Error)
    }

    fn u_start() -> FsmEvent {
        FromUpper(UserEvent::StartHandshake)
    }

    fn u_stop() -> FsmEvent {
        FromUpper(UserEvent::Stop)
    }

    fn u_re_rat() -> FsmEvent {
        FromUpper(UserEvent::RepeatRat)
    }

    fn u_data() -> FsmEvent {
        FromUpper(UserEvent::Data(vec![]))
    }

    #[test]
    fn fsm_transition_closed() {
        // locked should always stay in locked
        assert!(check_transition(locked(), locked(), p_msg(), Inactive));
        assert!(check_transition(locked(), locked(), p_failed(), Inactive));
        assert!(check_transition(locked(), locked(), p_ok(), Inactive));
        assert!(check_transition(locked(), locked(), v_msg(), Inactive));
        assert!(check_transition(locked(), locked(), v_failed(), Inactive));
        assert!(check_transition(locked(), locked(), v_ok(), Inactive));
        assert!(check_transition(locked(), locked(), u_data(), Inactive));
        assert!(check_transition(locked(), locked(), u_start(), Inactive));
        assert!(check_transition(locked(), locked(), u_stop(), Inactive));
        assert!(check_transition(locked(), locked(), u_re_rat(), Inactive));
        assert!(check_transition(locked(), locked(), sc_err(), Inactive));
        assert!(check_transition(
            locked(),
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(locked(), locked(), DatTimeout, Inactive));
        assert!(check_transition(locked(), locked(), RatTimeout, Inactive));
        //check all secure channel messages
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            locked(),
            locked(),
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));

        // unlocked should only allow lock and start
        assert!(check_transition(unlocked(), unlocked(), p_msg(), Inactive));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            p_failed(),
            Inactive
        ));
        assert!(check_transition(unlocked(), unlocked(), p_ok(), Inactive));
        assert!(check_transition(unlocked(), unlocked(), v_msg(), Inactive));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            v_failed(),
            Inactive
        ));
        assert!(check_transition(unlocked(), unlocked(), v_ok(), Inactive));
        assert!(check_transition(unlocked(), unlocked(), u_data(), Inactive));
        assert!(check_transition(
            unlocked(),
            WaitForHello,
            u_start(),
            Inactive
        ));
        assert!(check_transition(unlocked(), unlocked(), u_stop(), Inactive));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            u_re_rat(),
            Inactive
        ));
        //toDo assert!(check_transition(unlocked(), unlocked(), sc_data.clone()));
        assert!(check_transition(unlocked(), unlocked(), sc_err(), Inactive));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            unlocked(),
            unlocked(),
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_hello() {
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            p_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            v_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            u_start(),
            Inactive
        ));
        assert!(check_transition(WaitForHello, locked(), u_stop(), Inactive));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(WaitForHello, locked(), sc_err(), Inactive));
        assert!(check_transition(
            WaitForHello,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        //toDo add transitions for hello rat mechanism failed
        assert!(check_transition(
            WaitForHello,
            WaitForRat,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            locked(),
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForHello,
            WaitForHello,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_rat() {
        assert!(check_transition(WaitForRat, WaitForRat, p_msg(), Inactive));
        assert!(check_transition(WaitForRat, locked(), p_failed(), Inactive));
        assert!(check_transition(
            WaitForRat,
            WaitForRatVerifier,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(WaitForRat, WaitForRat, v_msg(), Inactive));
        assert!(check_transition(WaitForRat, locked(), v_failed(), Inactive));
        assert!(check_transition(
            WaitForRat,
            WaitForRatProver,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            u_start(),
            Inactive
        ));
        assert!(check_transition(WaitForRat, locked(), u_stop(), Inactive));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(WaitForRat, locked(), sc_err(), Inactive));
        assert!(check_transition(
            WaitForRat,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForDatAndRat,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRat, WaitForRat, RatTimeout, Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRat,
            WaitForRat,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_rat_p() {
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            locked(),
            p_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            Established,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            v_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            u_start(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            locked(),
            u_stop(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRat,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            locked(),
            sc_err(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForDatAndRat,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRat,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatProver,
            WaitForRatProver,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_rat_v() {
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            p_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            locked(),
            v_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            Established,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            u_start(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            locked(),
            u_stop(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            locked(),
            sc_err(),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForDatAndRatVerifier,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRat,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForRatVerifier,
            WaitForRat,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_dat_and_rat() {
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            p_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRatVerifier,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            v_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            u_start(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            u_stop(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            sc_err(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForRat,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            locked(),
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRat,
            WaitForDatAndRat,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_wait_for_dat_and_rat_v() {
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            p_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            p_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            v_failed(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            v_ok(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            u_start(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            locked(),
            u_stop(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            locked(),
            sc_err(),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            locked(),
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForRatVerifier,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            locked(),
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRat,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRatVerifier,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            WaitForDatAndRatVerifier,
            WaitForDatAndRat,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn fsm_transition_established() {
        assert!(check_transition(
            Established,
            Established,
            p_msg(),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            p_failed(),
            Inactive
        ));
        assert!(check_transition(Established, Established, p_ok(), Inactive));
        assert!(check_transition(
            Established,
            Established,
            v_msg(),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            v_failed(),
            Inactive
        ));
        assert!(check_transition(Established, Established, v_ok(), Inactive));
        assert!(check_transition(
            Established,
            WaitForAck,
            u_data(),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            u_start(),
            Inactive
        ));
        assert!(check_transition(Established, locked(), u_stop(), Inactive));
        assert!(check_transition(
            Established,
            WaitForRatVerifier,
            u_re_rat(),
            Inactive
        ));
        assert!(check_transition(Established, locked(), sc_err(), Inactive));
        assert!(check_transition(
            Established,
            Established,
            HandshakeTimeout,
            Inactive
        ));
        assert!(check_transition(
            Established,
            WaitForDatAndRatVerifier,
            DatTimeout,
            Inactive
        ));
        assert!(check_transition(
            Established,
            WaitForRatVerifier,
            RatTimeout,
            Inactive
        ));
        //check all secure channel messages
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_hello(
                Vec::from("valid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_hello(
                Vec::from("invalid"),
                &vec!["NullRat".to_owned()],
                &vec!["NullRat".to_owned()]
            )),
            Inactive
        ));
        assert!(check_transition(
            Established,
            locked(),
            get_sc_event(create_idscp_close(IdscpClose_CloseCause::ERROR, "")),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_dat(Vec::from("valid"))),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_dat(Vec::from("invalid"))),
            Inactive
        ));
        assert!(check_transition(
            Established,
            WaitForRatProver,
            get_sc_event(create_idscp_dat_exp()),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::new())),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_rat_verifier(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            Established,
            Established,
            get_sc_event(create_idscp_rat_prover(Vec::new())),
            Inactive
        ));
        assert!(check_transition(
            Established,
            WaitForRatProver,
            get_sc_event(create_idscp_re_rat("ReRat")),
            Inactive
        ));
    }

    #[test]
    fn test_all_transitions() {
        fsm_transition_closed();
        fsm_transition_wait_for_hello();
        fsm_transition_wait_for_rat();
        fsm_transition_wait_for_rat_p();
        fsm_transition_wait_for_rat_v();
        fsm_transition_wait_for_dat_and_rat();
        fsm_transition_wait_for_dat_and_rat_v();
        fsm_transition_established();
    }

    #[test]
    fn test_alternating_bit_sending() {
        let fsm = create_test_fsm(
            FsmState::Established,
            AckFlag::Active(vec![]),
            AlternatingBit::Zero,
            AlternatingBit::Zero,
        );
        let mut guard = fsm.lock().unwrap();
        let event = u_data();
        let _ = (*guard).process_event(event);
        assert_eq!((*guard).next_send_alternating_bit, AlternatingBit::Zero);
        assert_eq!((*guard).current_state, FsmState::WaitForAck);
        assert_eq!((*guard).ack_flag, AckFlag::Active(vec![]));

        let fsm = create_test_fsm(
            FsmState::Established,
            AckFlag::Active(vec![]),
            AlternatingBit::One,
            AlternatingBit::Zero,
        );
        let mut guard = fsm.lock().unwrap();
        let event = u_data();
        let _ = (*guard).process_event(event);
        assert_eq!((*guard).next_send_alternating_bit, AlternatingBit::One);
        assert_eq!((*guard).current_state, FsmState::WaitForAck);
        assert_eq!((*guard).ack_flag, AckFlag::Active(vec![]));
    }

    #[test]
    fn test_alternating_bit_in_ack() {
        let fsm = create_test_fsm(
            FsmState::WaitForAck,
            AckFlag::Active(vec![]),
            AlternatingBit::Zero,
            AlternatingBit::Zero,
        );
        let mut guard = fsm.lock().unwrap();
        let event = get_sc_event(create_idscp_ack(AlternatingBit::Zero));
        let _ = (*guard).process_event(event);
        assert_eq!((*guard).next_send_alternating_bit, AlternatingBit::One);
        assert_eq!((*guard).current_state, FsmState::Established);
        assert_eq!((*guard).ack_flag, AckFlag::Inactive);

        let fsm = create_test_fsm(
            FsmState::WaitForAck,
            AckFlag::Active(vec![]),
            AlternatingBit::Zero,
            AlternatingBit::Zero,
        );
        let mut guard = fsm.lock().unwrap();
        let event = get_sc_event(create_idscp_ack(AlternatingBit::One));
        let _ = (*guard).process_event(event);
        assert_eq!((*guard).next_send_alternating_bit, AlternatingBit::Zero);
        assert_eq!((*guard).current_state, FsmState::WaitForAck);
    }

    #[test]
    fn test_alternating_bit_receiving() {
        for state in &[FsmState::Established, FsmState::WaitForAck] {
            // expect 0, get 0, flip
            let fsm = create_test_fsm(
                state.clone(),
                AckFlag::Active(vec![]),
                AlternatingBit::Zero,
                AlternatingBit::Zero,
            );
            let mut guard = fsm.lock().unwrap();
            let event = get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::Zero));
            let _ = (*guard).process_event(event);
            assert_eq!((*guard).expected_alternating_bit, AlternatingBit::One);
            assert_eq!((*guard).current_state, state.clone());

            // expect 1, get 1, flip
            let fsm = create_test_fsm(
                state.clone(),
                AckFlag::Active(vec![]),
                AlternatingBit::Zero,
                AlternatingBit::One,
            );
            let mut guard = fsm.lock().unwrap();
            let event = get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::One));
            let _ = (*guard).process_event(event);
            assert_eq!((*guard).expected_alternating_bit, AlternatingBit::Zero);
            assert_eq!((*guard).current_state, state.clone());

            // expect 0, get 1, don't flip
            let fsm = create_test_fsm(
                state.clone(),
                AckFlag::Active(vec![]),
                AlternatingBit::Zero,
                AlternatingBit::Zero,
            );
            let mut guard = fsm.lock().unwrap();
            let event = get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::One));
            let _ = (*guard).process_event(event);
            assert_eq!((*guard).expected_alternating_bit, AlternatingBit::Zero);
            assert_eq!((*guard).current_state, state.clone());

            // expect 1, get 0, don't flip
            let fsm = create_test_fsm(
                state.clone(),
                AckFlag::Active(vec![]),
                AlternatingBit::Zero,
                AlternatingBit::One,
            );
            let mut guard = fsm.lock().unwrap();
            let event = get_sc_event(create_idscp_data(Vec::from("DATA"), &AlternatingBit::Zero));
            let _ = (*guard).process_event(event);
            assert_eq!((*guard).expected_alternating_bit, AlternatingBit::One);
            assert_eq!((*guard).current_state, state.clone());
        }
    }

    #[test]
    fn test_rat_algorithm_calculation() {
        let peer_rat_suites = ["C".to_string(), "B".to_string(), "A".to_string()];
        let own_rat_suites = ["B".to_string(), "C".to_string(), "D".to_string()];

        let rat_id =
            FiniteStateMachine::calculate_rat_prover_mechanism(&peer_rat_suites, &own_rat_suites)
                .unwrap();
        assert_eq!(rat_id, "C");

        let rat_id =
            FiniteStateMachine::calculate_rat_verifier_mechanism(&peer_rat_suites, &own_rat_suites)
                .unwrap();
        assert_eq!(rat_id, "B");
    }

    #[test]
    fn negative_test_rat_algorithm_calculation() {
        let peer_rat_suites = ["A".to_string(), "B".to_string()];
        let own_rat_suites = ["C".to_string(), "D".to_string()];

        assert_eq!(
            FiniteStateMachine::calculate_rat_prover_mechanism(&peer_rat_suites, &own_rat_suites),
            Err(RatNegotiationError::NoRatMechanismMatch)
        );

        assert_eq!(
            FiniteStateMachine::calculate_rat_verifier_mechanism(&peer_rat_suites, &own_rat_suites),
            Err(RatNegotiationError::NoRatMechanismMatch)
        );
    }
}
