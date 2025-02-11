use crate::state_api::state::HasStateLabel;
use crate::state_api::state::OpOut;
use crate::state_api::state::StateLabel;
use crate::OpId;
use crate::Operation;
use ic_config::execution_environment;
use ic_config::subnet_config::SubnetConfig;
use ic_crypto_sha2::Sha256;
use ic_ic00_types::CanisterInstallMode;
use ic_registry_subnet_type::SubnetType;
use ic_state_machine_tests::Cycles;
use ic_state_machine_tests::StateMachine;
use ic_state_machine_tests::StateMachineBuilder;
use ic_state_machine_tests::StateMachineConfig;
use ic_state_machine_tests::Time;
use ic_types::{CanisterId, PrincipalId};

pub struct PocketIc {
    subnet: StateMachine,
    nonce: u64,
    time: Time,
}

#[allow(clippy::new_without_default)]
impl PocketIc {
    pub fn new() -> Self {
        let hypervisor_config = execution_environment::Config {
            default_provisional_cycles_balance: Cycles::new(0),
            ..Default::default()
        };
        let config =
            StateMachineConfig::new(SubnetConfig::new(SubnetType::System), hypervisor_config);
        let sm = StateMachineBuilder::new()
            .with_config(Some(config))
            // essential for calculating state hashes
            // TODO: this degrades performance. enable only on demand.
            .with_checkpoints_enabled(true)
            .build();
        Self {
            subnet: sm,
            nonce: 0,
            time: Time::from_nanos_since_unix_epoch(0),
        }
    }
}

impl HasStateLabel for PocketIc {
    fn get_state_label(&self) -> StateLabel {
        let subnet_state_hash = self
            .subnet
            .state_manager
            .latest_state_certification_hash()
            .map(|(_, h)| h.0)
            .unwrap_or_else(|| [0u8; 32].to_vec());
        let mut hasher = Sha256::new();
        hasher.write(&subnet_state_hash[..]);
        hasher.write(&self.nonce.to_be_bytes());
        hasher.write(&self.time.as_nanos_since_unix_epoch().to_be_bytes());
        StateLabel(hasher.finish())
    }
}

// ---------------------------------------------------------------------------------------- //
// Operations on PocketIc

#[derive(Clone, Debug)]
pub struct SetTime {
    pub time: Time,
}

impl Operation for SetTime {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        // set time for all subnets; but also for the whole PocketIC
        // subnets won't have their own time field in the future.
        pic.subnet.set_time(self.time.into());
        pic.time = self.time;
        OpOut::NoOutput
    }

    fn id(&self) -> OpId {
        OpId(format!("set_time_{}", self.time))
    }
}

#[derive(Clone, Debug)]
pub struct GetTime {}

impl Operation for GetTime {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        // get time from PocketIC, not from subnet:
        OpOut::Time(pic.time.as_nanos_since_unix_epoch())
    }

    fn id(&self) -> OpId {
        OpId("get_time".into())
    }
}

#[derive(Clone, Debug)]
pub struct Tick {}

impl Operation for Tick {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        pic.subnet.tick();
        OpOut::NoOutput
    }

    fn id(&self) -> OpId {
        OpId("tick".to_string())
    }
}

#[derive(Clone, Debug)]
pub struct ExecuteIngressMessage(pub CanisterCall);

impl Operation for ExecuteIngressMessage {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        pic.subnet
            .execute_ingress_as(
                self.0.sender,
                self.0.canister_id,
                self.0.method,
                self.0.payload,
            )
            .into()
    }

    fn id(&self) -> OpId {
        let call_id = self.0.id();
        OpId(format!("canister_update_{}", call_id.0))
    }
}

pub struct Query(pub CanisterCall);

impl Operation for Query {
    type TargetType = PocketIc;
    fn compute(self, pic: &mut PocketIc) -> OpOut {
        pic.subnet
            .query_as(
                self.0.sender,
                self.0.canister_id,
                self.0.method,
                self.0.payload,
            )
            .into()
    }

    fn id(&self) -> OpId {
        let call_id = self.0.id();
        OpId(format!("canister_query_{}", call_id.0))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CanisterCall {
    pub sender: PrincipalId,
    pub canister_id: CanisterId,
    pub method: String,
    pub payload: Vec<u8>,
}

impl CanisterCall {
    fn id(&self) -> OpId {
        let mut hasher = Sha256::new();
        hasher.write(&self.payload);
        let hash = Digest(hasher.finish());
        OpId(format!(
            "call({},{},{},{})",
            self.sender, self.canister_id, self.method, hash
        ))
    }
}

/// A convenience method that installs the given wasm module at the given canister id. The first
/// controller of the given canister is set as the sender. If the canister has no controller set,
/// the anynmous user is used.
pub struct InstallCanisterAsController {
    pub canister_id: CanisterId,
    pub mode: CanisterInstallMode,
    pub module: Vec<u8>,
    pub payload: Vec<u8>,
}

impl Operation for InstallCanisterAsController {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        pic.subnet
            .install_wasm_in_mode(self.canister_id, self.mode, self.module, self.payload)
            .into()
    }

    fn id(&self) -> OpId {
        OpId("".into())
    }
}

#[derive(Clone, Debug)]
pub struct CyclesBalance {
    canister_id: CanisterId,
}

impl Operation for CyclesBalance {
    type TargetType = PocketIc;
    fn compute(self, pic: &mut PocketIc) -> OpOut {
        let result = pic.subnet.cycle_balance(self.canister_id);
        OpOut::Cycles(result)
    }

    fn id(&self) -> OpId {
        OpId(format!("cycles_balance({})", self.canister_id))
    }
}

/// Add cycles to a given canister.
///
/// # Panics
///
/// Panics if the canister does not exist.
#[derive(Clone, Debug)]
pub struct AddCycles {
    canister_id: CanisterId,
    amount: u128,
}

impl Operation for AddCycles {
    type TargetType = PocketIc;

    fn compute(self, pic: &mut PocketIc) -> OpOut {
        let result = pic.subnet.add_cycles(self.canister_id, self.amount);
        OpOut::Cycles(result)
    }

    fn id(&self) -> OpId {
        OpId(format!("add_cycles({},{})", self.canister_id, self.amount))
    }
}

struct Digest([u8; 32]);

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Digest(")?;
        self.0.iter().try_for_each(|b| write!(f, "{:02X}", b))?;
        write!(f, ")")
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_state_machine_tests::WasmResult;

    #[test]
    fn state_label_test() {
        let pic = PocketIc::new();

        let state0 = pic.get_state_label();
        let canister_id = pic.subnet.create_canister(None);
        let state1 = pic.get_state_label();
        let _ = pic.subnet.delete_canister(canister_id);
        let state2 = pic.get_state_label();

        assert!(state0 != state1);
        assert!(state1 != state2);
        assert!(state0 != state2);
    }

    #[test]
    fn test_time() {
        let mut pic = PocketIc::new();

        let time = Time::from_nanos_since_unix_epoch(21);
        compute_assert_state_change(&mut pic, SetTime { time });
        let expected_time = OpOut::Time(21);
        let actual_time = compute_assert_state_immutable(&mut pic, GetTime {});

        assert_eq!(expected_time, actual_time);
    }

    #[test]
    fn test_execute_message() {
        let (mut pic, canister_id) = new_pic_counter_installed();

        let update = ExecuteIngressMessage(CanisterCall {
            sender: PrincipalId::new_anonymous(),
            canister_id,
            method: "write".into(),
            payload: vec![],
        });

        compute_assert_state_change(&mut pic, update);
    }

    #[test]
    fn test_query() {
        let (mut pic, canister_id) = new_pic_counter_installed();
        let (query, update) = query_update_constructors(canister_id);

        use WasmResult::*;
        let OpOut::WasmResult(Reply(initial_bytes)) =
            compute_assert_state_immutable(&mut pic, query("read"))
        else {
            unreachable!()
        };
        compute_assert_state_change(&mut pic, update("write"));
        let OpOut::WasmResult(Reply(updated_bytes)) =
            compute_assert_state_immutable(&mut pic, query("read"))
        else {
            unreachable!()
        };

        assert_eq!(updated_bytes[0], initial_bytes[0] + 1);
    }

    #[test]
    fn test_cycles() {
        let (mut pic, canister_id) = new_pic_counter_installed();
        let (_, update) = query_update_constructors(canister_id);

        let cycles_balance = CyclesBalance { canister_id };
        let OpOut::Cycles(orig_balance) =
            compute_assert_state_immutable(&mut pic, cycles_balance.clone())
        else {
            unreachable!()
        };
        compute_assert_state_change(&mut pic, update("write"));
        let OpOut::Cycles(changed_balance) =
            compute_assert_state_immutable(&mut pic, cycles_balance)
        else {
            unreachable!()
        };

        // nothing is charged on a system subnet
        assert_eq!(changed_balance, orig_balance);

        let amount: u128 = 20_000_000_000_000;
        let add_cycles = AddCycles {
            canister_id,
            amount,
        };

        let OpOut::Cycles(final_balance) = compute_assert_state_change(&mut pic, add_cycles) else {
            unreachable!()
        };

        assert_eq!(final_balance, changed_balance + amount);
    }

    fn query_update_constructors(
        canister_id: CanisterId,
    ) -> (
        impl Fn(&str) -> Query,
        impl Fn(&str) -> ExecuteIngressMessage,
    ) {
        let call = move |method: &str| CanisterCall {
            sender: PrincipalId::new_anonymous(),
            canister_id,
            method: method.into(),
            payload: vec![],
        };

        let update = move |m: &str| ExecuteIngressMessage(call(m));
        let query = move |m: &str| Query(call(m));

        (query, update)
    }

    fn new_pic_counter_installed() -> (PocketIc, CanisterId) {
        let mut pic = PocketIc::new();
        let canister_id = pic.subnet.create_canister(None);

        let module = counter_wasm();
        let install_op = InstallCanisterAsController {
            canister_id,
            mode: CanisterInstallMode::Install,
            module,
            payload: vec![],
        };

        compute_assert_state_change(&mut pic, install_op);

        (pic, canister_id)
    }

    fn compute_assert_state_change<O>(pic: &mut PocketIc, op: O) -> OpOut
    where
        O: Operation<TargetType = PocketIc>,
    {
        let state0 = pic.get_state_label();
        let res = op.compute(pic);
        let state1 = pic.get_state_label();
        assert!(state0 != state1);
        res
    }

    fn compute_assert_state_immutable<O>(pic: &mut PocketIc, op: O) -> OpOut
    where
        O: Operation<TargetType = PocketIc>,
    {
        let state0 = pic.get_state_label();
        let res = op.compute(pic);
        let state1 = pic.get_state_label();
        assert_eq!(state0, state1);
        res
    }

    fn counter_wasm() -> Vec<u8> {
        wat::parse_str(COUNTER_WAT).unwrap().as_slice().to_vec()
    }

    const COUNTER_WAT: &str = r#"
;; Counter with global variable ;;
(module
  (import "ic0" "msg_reply" (func $msg_reply))
  (import "ic0" "msg_reply_data_append"
    (func $msg_reply_data_append (param i32 i32)))

  (func $read
    (i32.store
      (i32.const 0)
      (global.get 0)
    )
    (call $msg_reply_data_append
      (i32.const 0)
      (i32.const 4))
    (call $msg_reply))

  (func $write
    (global.set 0
      (i32.add
        (global.get 0)
        (i32.const 1)
      )
    )
    (call $read)
  )

  (memory $memory 1)
  (export "memory" (memory $memory))
  (global (export "counter_global") (mut i32) (i32.const 0))
  (export "canister_query read" (func $read))
  (export "canister_query inc_read" (func $write))
  (export "canister_update write" (func $write))
)
    "#;
}
