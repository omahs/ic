use super::*;
use crate::{
    governance::{Governance, MockEnvironment},
    pb::v1::Governance as GovernanceProto,
};
use ic_nervous_system_common::{cmc::MockCMC, ledger::MockIcpLedger};
use maplit::btreemap;
use std::time::{SystemTime, UNIX_EPOCH};

fn simple_neuron(id: u64) -> Neuron {
    // Make sure different neurons have different accounts.
    let mut account = vec![0; 32];
    for (destination, data) in account.iter_mut().zip(id.to_le_bytes().iter().cycle()) {
        *destination = *data;
    }

    Neuron {
        id: Some(NeuronId { id }),
        account,
        ..Default::default()
    }
}

// The following tests are not verifying the content of the stable indexes yet, as it's currently
// impossible to read from the indexes through its pub API. Those should be added when we start to
// allow reading from the stable indexes.
#[test]
fn test_batch_add_heap_neurons_to_stable_indexes_two_batches() {
    let mut neuron_store = NeuronStore::new(btreemap! {
        1 => simple_neuron(1),
        3 => simple_neuron(3),
        7 => simple_neuron(7),
    });

    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 0 }, 2),
        Ok(Some(NeuronId { id: 3 }))
    );
    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 3 }, 2),
        Ok(None)
    );
}

#[test]
fn test_batch_add_heap_neurons_to_stable_indexes_three_batches_last_empty() {
    let mut neuron_store = NeuronStore::new(btreemap! {
        1 => simple_neuron(1),
        3 => simple_neuron(3),
        7 => simple_neuron(7),
        12 => simple_neuron(12),
    });

    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 0 }, 2),
        Ok(Some(NeuronId { id: 3 }))
    );
    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 3 }, 2),
        Ok(Some(NeuronId { id: 12 }))
    );
    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 12 }, 2),
        Ok(None)
    );
}

#[test]
fn test_batch_add_heap_neurons_to_stable_indexes_failure() {
    let mut neuron_store = NeuronStore::new(btreemap! {
        1 => simple_neuron(1),
    });

    assert_eq!(
        neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 0 }, 2),
        Ok(None)
    );

    // Calling it again ignoring the progress would cause a failure.
    let result = neuron_store.batch_add_heap_neurons_to_stable_indexes(NeuronId { id: 0 }, 2);
    assert!(result.is_err(), "{:?}", result);
    let error = result.err().unwrap();
    assert!(error.contains("Subaccount"), "{}", error);
    assert!(error.contains("already exists in the index"), "{}", error);
}

#[test]
fn test_batch_add_inactive_neurons_to_stable_memory() {
    // Step 1: Prepare the world.

    // Each element is (Neuron, is inactive).
    let batch = vec![
        (simple_neuron(1), false),
        (simple_neuron(3), true),
        (simple_neuron(7), false),
        (simple_neuron(12), true),
    ];

    // This isn't actually used, but we do this for realism.
    let id_to_neuron = BTreeMap::from_iter(batch.iter().map(|(neuron, _is_active)| {
        let neuron = neuron.clone();
        let id = neuron.id.as_ref().unwrap().id;

        (id, neuron)
    }));

    // No need to clear STABLE_NEURON_STORE, because each #[test] is run in its
    // own thread.

    // Step 2: Call the code under test.
    let mut neuron_store = NeuronStore::new(id_to_neuron);
    let batch_result = neuron_store.batch_add_inactive_neurons_to_stable_memory(batch);

    // Step 3: Verify.

    let last_neuron_id = NeuronId { id: 12 };
    assert_eq!(batch_result, Ok(Some(last_neuron_id)));

    fn read(neuron_id: NeuronId) -> Result<Neuron, GovernanceError> {
        STABLE_NEURON_STORE.with(|s| s.borrow().read(neuron_id))
    }

    // Step 3.1: Assert that neurons 3 and 12 were copied, since they are inactive.
    for neuron_id in [3, 12] {
        let neuron_id = NeuronId { id: neuron_id };

        let read_result = read(neuron_id);

        match &read_result {
            Ok(ok) => assert_eq!(ok, &simple_neuron(neuron_id.id)),
            _ => panic!("{:?}", read_result),
        }
    }

    // Step 3.2: Assert that other neurons were NOT copied, since they are active.
    for neuron_id in 1..10 {
        // Skip inactive neuron IDs.
        if [3, 12].contains(&neuron_id) {
            continue;
        }

        let neuron_id = NeuronId { id: neuron_id };

        let read_result = read(neuron_id);

        match &read_result {
            Err(err) => {
                let GovernanceError {
                    error_type,
                    error_message,
                } = err;

                assert_eq!(
                    ErrorType::from_i32(*error_type),
                    Some(ErrorType::NotFound),
                    "{:?}",
                    err
                );

                let error_message = error_message.to_lowercase();
                assert!(error_message.contains("unable"), "{:?}", err);
                assert!(
                    error_message.contains(&format!("{}", neuron_id.id)),
                    "{:?}",
                    err
                );
            }

            _ => panic!("{:#?}", read_result),
        }
    }
}

#[test]
fn test_heap_range_with_begin_and_limit() {
    let neuron_store = NeuronStore::new(btreemap! {
        1 => simple_neuron(1),
        3 => simple_neuron(3),
        7 => simple_neuron(7),
        12 => simple_neuron(12),
    });

    let observed_neurons: Vec<_> = neuron_store
        .range_heap_neurons(NeuronId { id: 3 }..)
        .take(2)
        .collect();

    assert_eq!(observed_neurons, vec![simple_neuron(3), simple_neuron(7)],);
}

#[test]
fn test_with_neuron_mut_inactive_neuron() {
    // Step 1: Prepare the world.

    // Step 1.1: The main characters: a couple of Neurons, one active, the other inactive.
    let funded_neuron = Neuron {
        id: Some(NeuronId { id: 42 }),
        cached_neuron_stake_e8s: 1, // Funded. Thus, no stable memory.
        ..Default::default()
    };
    let funded_neuron_id = funded_neuron.id.unwrap();

    let unfunded_neuron = Neuron {
        id: Some(NeuronId { id: 777 }),
        cached_neuron_stake_e8s: 0, // Unfunded. Thus, should be copied to stable memory.
        ..Default::default()
    };
    let unfunded_neuron_id = unfunded_neuron.id.unwrap();

    // Make sure our test data is correct. Here, we use dummy values for proposals and
    // in_flight_commands.
    {
        let proposals = Default::default();
        let in_flight_commands = Default::default();
        let is_neuron_inactive =
            |neuron: &Neuron| neuron.is_inactive(&proposals, &in_flight_commands);
        assert!(is_neuron_inactive(&unfunded_neuron), "{:#?}", funded_neuron);
        assert!(!is_neuron_inactive(&funded_neuron), "{:#?}", funded_neuron);
    }

    // Step 1.2: Construct collaborators of Governance, and Governance itself.
    let mut governance = {
        let governance_proto = GovernanceProto {
            neurons: btreemap! {
                funded_neuron_id.id => funded_neuron.clone(),
                unfunded_neuron_id.id => unfunded_neuron.clone(),
            },
            ..Default::default()
        };

        // Governance::new calls environment.now. This just part of "preparing the world", not the
        // code under test itself. Nevertheless, we have to tell the `mockall` crate about this;
        // otherwise it will freak out.
        let mut environment = MockEnvironment::new();
        let now_timestamp_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        environment.expect_now().return_const(now_timestamp_seconds);

        Governance::new(
            governance_proto,
            Box::new(environment),
            Box::new(MockIcpLedger::new()),
            Box::new(MockCMC::new()),
        )
    };

    // Step 2: Call the code under test (twice).
    let results = [funded_neuron_id, unfunded_neuron_id].map(|neuron_id| {
        governance.with_neuron_mut(&neuron_id, |neuron: &mut Neuron| {
            // Modify the neuron a little bit.
            neuron.account = vec![1, 2, 3];

            // Don't just return () so that the return value has something
            // (such as it is) to inspect.
            ("ok", neuron_id)
        })
    });

    // Step 3: Verify result(s).
    assert_eq!(results.len(), 2, "{:#?}", results); // A sanity check.
    for result in results {
        let neuron_id = result.as_ref().map(|(_ok, neuron_id)| *neuron_id).unwrap();
        assert_eq!(result, Ok(("ok", neuron_id)));
    }

    // Step 3.1: The main thing that we want to see is that the unfunded Neuron ends up in stable
    // memory (and has the modification).
    assert_eq!(
        STABLE_NEURON_STORE
            .with(|stable_neuron_store| { stable_neuron_store.borrow().read(unfunded_neuron_id) }),
        Ok(Neuron {
            account: vec![1, 2, 3],
            ..unfunded_neuron
        }),
    );

    // Step 3.2: Negative result: funded neuron should not be copied to stable memory. Perhaps, less
    // interesting, but also important is that some neurons (to wit, the funded Neuron) do NOT get
    // copied to stable memory.
    let funded_neuron_read_result = STABLE_NEURON_STORE
        .with(|stable_neuron_store| stable_neuron_store.borrow().read(funded_neuron_id));
    match &funded_neuron_read_result {
        Ok(_ok) => {
            panic!(
                "Seems that the funded neuron was copied to stable memory. Result:\n{:#?}",
                funded_neuron_read_result,
            );
        }

        Err(err) => {
            let GovernanceError {
                error_type,
                error_message,
            } = err;

            let error_type = ErrorType::from_i32(*error_type);
            assert_eq!(error_type, Some(ErrorType::NotFound), "{:#?}", err);

            let error_message = error_message.to_lowercase();
            assert!(error_message.contains("unable"), "{:#?}", err);
            assert!(error_message.contains("find"), "{:#?}", err);
            assert!(
                error_message.contains(&format!("{}", funded_neuron_id.id)),
                "{:#?}",
                err
            );
        }
    }
}
