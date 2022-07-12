use super::debug_info::DebugInfo;
use super::{halt_locations::HaltLocations, DebugError};
use crate::{core::Core, CoreStatus};

/// Stepping granularity for stepping through a program during debug.
#[derive(Debug)]
pub enum SteppingMode {
    /// Advance one machine instruction at a time.
    StepInstruction,
    /// Step Over the current statement, and halt at the start of the next statement.
    OverStatement,
    /// Use best efforts to determin the location of any function calls in this statement, and step into them.
    IntoStatement,
    /// Step to the calling statement, immediately after the current function returns.
    OutOfStatement,
}

impl SteppingMode {
    /// Determine the program counter location where the SteppingMode is aimed, and step to it.
    /// Return the new CoreStatus and program_counter value.
    ///
    /// Implementation Notes for stepping at statement granularity:
    /// - If a hardware breakpoint is available, we will set it at the desired location, run to it, and release it.
    /// - If no hardware breakpoints are available, we will do repeated instruction steps until we reach the desired location.
    ///
    /// Usage Note:
    /// - Currently, no special provision is made for the effect of interrupts that get triggered during stepping. The user must ensure that interrupts are disabled during stepping, or accept that stepping may be diverted by the interrupt processing on the core.
    pub fn step(
        &self,
        core: &mut Core<'_>,
        debug_info: &DebugInfo,
    ) -> Result<(CoreStatus, u64), DebugError> {
        let mut core_status = core
            .status()
            .map_err(|error| DebugError::Other(anyhow::anyhow!(error)))
            .map_err(|error| DebugError::Other(anyhow::anyhow!(error)))?;
        let (mut program_counter, mut return_address) = match core_status {
            CoreStatus::Halted(_) => (
                core.read_core_reg(core.registers().program_counter())?,
                core.read_core_reg(core.registers().return_address())?,
            ),
            _ => {
                return Err(DebugError::Other(anyhow::anyhow!(
                    "Core must be halted before stepping."
                )))
            }
        };

        // First deal with the the fast/easy case.
        if matches!(self, SteppingMode::StepInstruction) {
            program_counter = core.step()?.pc;
            core_status = core.status()?;
            return Ok((core_status, program_counter));
        }

        let mut target_address: Option<u64> = None;
        let mut adjusted_program_counter = program_counter;

        // Sometimes the target program_counter is at a location where the debug_info program row data does not contain valid statements for halt points.
        // When DebugError::NoValidHaltLocation happens, we will step to the next instruction and try again(until we can reasonably expect to have passed out of an epilogue), before giving up.
        for _ in 0..10 {
            match HaltLocations::new(debug_info, program_counter, Some(return_address)) {
                Ok(program_row_data) => {
                    match self {
                        SteppingMode::IntoStatement => {
                            // This is a tricky case, for a couple of reasons:
                            // - Firstly, the current RUST generated DWARF, does not store the DW_TAG_call_site information described in the DWARF 5 standard. It is not a mandatory attribute, so not sure if we can ever expect it.
                            // - Secondly, because we want the first_halt_address to be based on the instructions of the called function, while next_halt_address and step_out_address must be based on the current sequence of statements.

                            // To find (if they exist) functions called from the current program counter:
                            // - At the current PC, determine the `next_statement_address` in the current sequence of instructions are.
                            // - Single step the target core, until either ...
                            //   (a) We hit a PC that is not in the sequence between starting PC and the address of the `next_statement_address` stored above. Halt at this location.
                            //      (a.i) This could mean either that we encountered a branch (call to another instruction), or an interrupt handler diverted the processing.
                            //   (b) The new PC matches the next valid statement stored above, which means there was nothing to step into, so the target is now halted (correctly) at the `next_halt_address`

                            target_address = if let (
                                Some(first_halt_location),
                                Some(next_statement_address),
                            ) = (
                                program_row_data.first_halt_address,
                                program_row_data.next_statement_address,
                            ) {
                                adjusted_program_counter = loop {
                                    let next_pc = core.step()?.pc;
                                    if (first_halt_location..next_statement_address)
                                        .contains(&next_pc)
                                    {
                                        // We are still in the current sequence of instructions, before the next_statement_address.
                                        continue;
                                    } else if next_pc == next_statement_address {
                                        // We have reached the next_statement_address, so we can conclude there was no branching calls in this sequence.
                                        log::warn!("Stepping into next statement, but no branching calls found. Stepped to next available statement.");
                                        break next_pc;
                                    } else {
                                        // We have reached a location that is not in the current sequence, so we can conclude there was a branching call in this sequence.
                                        if let Some(valid_halt_address) =
                                            HaltLocations::new(debug_info, next_pc, None)
                                                .ok()
                                                .and_then(|program_row_data| {
                                                    program_row_data.next_statement_address
                                                })
                                        {
                                            break valid_halt_address;
                                        } else {
                                            break next_pc;
                                        }
                                    }
                                };
                                Some(adjusted_program_counter)
                            } else {
                                // Our technique requires a valid first_halt_address AND a valid next_statement_address, so if we don't have one, we will later on step a single instruction.
                                None
                            };

                            if target_address.is_none() {
                                log::error!(
                                    "Unable to determine target functions for stepping into instructions at {:x}. Stepping to next target instruction.",
                                    program_counter
                                );
                                program_counter = core.step()?.pc;
                                core_status = core.status()?;
                                return Ok((core_status, program_counter));
                            }
                        }
                        SteppingMode::OverStatement => {
                            target_address = program_row_data.next_statement_address
                        }
                        SteppingMode::OutOfStatement => {
                            if program_row_data.step_out_address.is_none() {
                                return Err(DebugError::NoValidHaltLocation {
                                    message: "Cannot step out of a non-returning function"
                                        .to_string(),
                                    pc_at_error: program_counter as u64,
                                });
                            } else {
                                target_address = program_row_data.step_out_address
                            }
                        }
                        _ => {
                            // We've already covered SteppingMode::StepInstruction
                        }
                    }
                    // If we get here, we don't have to retry anymore.
                    break;
                }
                Err(error) => match error {
                    DebugError::NoValidHaltLocation {
                        message,
                        pc_at_error,
                    } => {
                        // Step on target instruction, and then try again.
                        log::debug!(
                            "Incomplete stepping information @{:#010X}: {}",
                            pc_at_error,
                            message
                        );
                        program_counter = core.step()?.pc;
                        return_address = core.read_core_reg(core.registers().return_address())?;
                        continue;
                    }
                    other_error => return Err(other_error),
                },
            }
        }
        match target_address {
            Some(target_address) => {
                log::debug!(
                    "Preparing to step ({:20?}) from: {:#010X} to: {:#010X}",
                    self,
                    program_counter,
                    target_address
                );

                if target_address == adjusted_program_counter as u64 {
                    // For inline functions we have already stepped to the correct target address..
                } else if core.set_hw_breakpoint(target_address).is_ok() {
                    core.run()?;
                    core.clear_hw_breakpoint(target_address)?;
                    core_status = match core.status() {
                        Ok(core_status) => {
                            match core_status {
                                CoreStatus::Halted(_) => {
                                    adjusted_program_counter =
                                        core.read_core_reg(core.registers().program_counter())?
                                }
                                other => {
                                    log::error!(
                                        "Core should be halted after stepping but is: {:?}",
                                        &other
                                    );
                                    adjusted_program_counter = 0;
                                }
                            };
                            core_status
                        }
                        Err(error) => return Err(DebugError::Probe(error)),
                    };
                } else {
                    while target_address != core.step()?.pc {
                        // Single step the core until we get to the target_address;
                        // TODO: In theory, this could go on for a long time. Should we consider NOT allowing this kind of stepping if there are no breakpoints available?
                    }
                    core_status = core.status()?;
                    adjusted_program_counter = target_address;
                }
            }
            None => {
                return Err(DebugError::NoValidHaltLocation {
                    message: "Unable to determine target address for this step request."
                        .to_string(),
                    pc_at_error: program_counter as u64,
                });
            }
        }
        Ok((core_status, adjusted_program_counter))
    }
}
