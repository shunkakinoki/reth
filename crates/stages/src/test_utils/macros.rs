macro_rules! stage_test_suite {
    ($runner:ident, $name:ident) => {

         paste::item! {
            /// Check that the execution is short-circuited if the database is empty.
            #[tokio::test]
            async fn [< execute_empty_db_ $name>] () {
                // Set up the runner
                let runner = $runner::default();

                // Execute the stage with empty database
                let input = crate::stage::ExecInput::default();

                // Run stage execution
                let result = runner.execute(input).await;
                // Check that the result is returned and the stage does not panic.
                // The return result with empty db is stage-specific.
                assert_matches::assert_matches!(result, Ok(_));

                // Validate the stage execution
                assert_matches::assert_matches!(runner.validate_execution(input, result.unwrap().ok()),Ok(_), "execution validation");
            }

            // Run the complete stage execution flow.
            #[tokio::test]
            async fn [< execute_ $name>] () {
                let (previous_stage, stage_progress) = (500, 100);

                // Set up the runner
                let mut runner = $runner::default();
                let input = crate::stage::ExecInput {
                    target: Some(previous_stage),
                    checkpoint: Some(reth_primitives::stage::StageCheckpoint::new(stage_progress)),
                };
                let seed = runner.seed_execution(input).expect("failed to seed");
                let rx = runner.execute(input);

                // Run `after_execution` hook
                runner.after_execution(seed).await.expect("failed to run after execution hook");

                // Assert the successful result
                let result = rx.await.unwrap();
                assert_matches::assert_matches!(
                    result,
                    Ok(ref output @ ExecOutput { checkpoint })
                        if output.is_done(input) && checkpoint.block_number == previous_stage
                );

                // Validate the stage execution
                assert_matches::assert_matches!(runner.validate_execution(input, result.ok()),Ok(_), "execution validation");
            }

            // Check that unwind does not panic on no new entries within the input range.
            #[tokio::test]
            async fn [< unwind_no_new_entries_ $name>] () {
                // Set up the runner
                let mut runner = $runner::default();
                let input = crate::stage::UnwindInput::default();

                // Seed the database
                runner.seed_execution(crate::stage::ExecInput::default()).expect("failed to seed");

                runner.before_unwind(input).expect("failed to execute before_unwind hook");

                // Run stage unwind
                let rx = runner.unwind(input).await;
                assert_matches::assert_matches!(
                    rx,
                    Ok(UnwindOutput { checkpoint }) if checkpoint.block_number == input.unwind_to
                );

                // Validate the stage unwind
                assert_matches::assert_matches!(runner.validate_unwind(input),Ok(_), "unwind validation");
            }

            // Run complete execute and unwind flow.
            #[tokio::test]
            async fn [< unwind_ $name>] () {
                let (previous_stage, stage_progress) = (500, 100);

                // Set up the runner
                let mut runner = $runner::default();
                let execute_input = crate::stage::ExecInput {
                    target: Some(previous_stage),
                    checkpoint: Some(reth_primitives::stage::StageCheckpoint::new(stage_progress)),
                };
                let seed = runner.seed_execution(execute_input).expect("failed to seed");

                // Run stage execution
                let rx = runner.execute(execute_input);
                runner.after_execution(seed).await.expect("failed to run after execution hook");

                // Assert the successful execution result
                let result = rx.await.unwrap();
                assert_matches::assert_matches!(
                    result,
                    Ok(ref output @ ExecOutput { checkpoint })
                        if output.is_done(execute_input) && checkpoint.block_number == previous_stage
                );
                assert_matches::assert_matches!(runner.validate_execution(execute_input, result.ok()),Ok(_), "execution validation");


                // Run stage unwind
                let unwind_input = crate::stage::UnwindInput {
                    unwind_to: stage_progress,
                    checkpoint: reth_primitives::stage::StageCheckpoint::new(previous_stage),
                    bad_block: None,
                };

                runner.before_unwind(unwind_input).expect("Failed to unwind state");

                let rx = runner.unwind(unwind_input).await;
                // Assert the successful unwind result
                assert_matches::assert_matches!(
                    rx,
                    Ok(output @ UnwindOutput { checkpoint })
                        if output.is_done(unwind_input) && checkpoint.block_number == unwind_input.unwind_to
                );

                // Validate the stage unwind
                assert_matches::assert_matches!(runner.validate_unwind(unwind_input),Ok(_), "unwind validation");
            }
        }
    };
}

pub(crate) use stage_test_suite;
