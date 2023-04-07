//! A tx for updating an account's validity predicate.
//! This tx wraps the validity predicate inside `SignedTxData` as
//! its input as declared in `shared` crate.

use namada_tx_prelude::*;

#[transaction]
fn apply_tx(ctx: &mut Ctx, tx_data: Tx) -> TxResult {
    let signed = tx_data;
    let data = signed.data().ok_or_err_msg("Missing data")?;
    let update_vp = transaction::UpdateVp::try_from_slice(&data[..])
        .wrap_err("failed to decode UpdateVp")?;

    debug_log!("update VP for: {:#?}", update_vp.addr);

    let vp_code = signed.get_section(&update_vp.vp_code)
        .ok_or_err_msg("vp code section not found")?
        .extra_data()
        .ok_or_err_msg("vp code section must be tagged as extra")?
        .data;
    ctx.update_validity_predicate(
        &update_vp.addr,
        vp_code,
    )
}
