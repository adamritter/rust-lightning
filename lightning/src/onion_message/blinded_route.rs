// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Creating blinded routes and related utilities live here.

use bitcoin::secp256k1::{self, PublicKey, Secp256k1, SecretKey};

use chain::keysinterface::{KeysInterface, Sign};
use super::utils;
use ln::msgs::DecodeError;
use util::chacha20poly1305rfc::ChaChaPolyWriteAdapter;
use util::ser::{Readable, VecWriter, Writeable, Writer};

use io;
use prelude::*;

/// Onion messages can be sent and received to blinded routes, which serve to hide the identity of
/// the recipient.
pub struct BlindedRoute {
	/// To send to a blinded route, the sender first finds a route to the unblinded
	/// `introduction_node_id`, which can unblind its [`encrypted_payload`] to find out the onion
	/// message's next hop and forward it along.
	///
	/// [`encrypted_payload`]: BlindedHop::encrypted_payload
	pub(super) introduction_node_id: PublicKey,
	/// Used by the introduction node to decrypt its [`encrypted_payload`] to forward the onion
	/// message.
	///
	/// [`encrypted_payload`]: BlindedHop::encrypted_payload
	pub(super) blinding_point: PublicKey,
	/// The hops composing the blinded route.
	pub(super) blinded_hops: Vec<BlindedHop>,
}

/// Used to construct the blinded hops portion of a blinded route. These hops cannot be identified
/// by outside observers and thus can be used to hide the identity of the recipient.
pub struct BlindedHop {
	/// The blinded node id of this hop in a blinded route.
	pub(super) blinded_node_id: PublicKey,
	/// The encrypted payload intended for this hop in a blinded route.
	// The node sending to this blinded route will later encode this payload into the onion packet for
	// this hop.
	pub(super) encrypted_payload: Vec<u8>,
}

impl BlindedRoute {
	/// Create a blinded route to be forwarded along `node_pks`. The last node pubkey in `node_pks`
	/// will be the destination node.
	///
	/// Errors if less than two hops are provided or if `node_pk`(s) are invalid.
	//  TODO: make all payloads the same size with padding + add dummy hops
	pub fn new<Signer: Sign, K: KeysInterface, T: secp256k1::Signing + secp256k1::Verification>
		(node_pks: &[PublicKey], keys_manager: &K, secp_ctx: &Secp256k1<T>) -> Result<Self, ()>
	{
		if node_pks.len() < 2 { return Err(()) }
		let blinding_secret_bytes = keys_manager.get_secure_random_bytes();
		let blinding_secret = SecretKey::from_slice(&blinding_secret_bytes[..]).expect("RNG is busted");
		let introduction_node_id = node_pks[0];

		Ok(BlindedRoute {
			introduction_node_id,
			blinding_point: PublicKey::from_secret_key(secp_ctx, &blinding_secret),
			blinded_hops: blinded_hops(secp_ctx, node_pks, &blinding_secret).map_err(|_| ())?,
		})
	}
}

/// Construct blinded hops for the given `unblinded_path`.
fn blinded_hops<T: secp256k1::Signing + secp256k1::Verification>(
	secp_ctx: &Secp256k1<T>, unblinded_path: &[PublicKey], session_priv: &SecretKey
) -> Result<Vec<BlindedHop>, secp256k1::Error> {
	let mut blinded_hops = Vec::with_capacity(unblinded_path.len());

	let mut prev_ss_and_blinded_node_id = None;
	utils::construct_keys_callback(secp_ctx, unblinded_path, None, session_priv, |blinded_node_id, _, _, encrypted_payload_ss, unblinded_pk, _| {
		if let Some((prev_ss, prev_blinded_node_id)) = prev_ss_and_blinded_node_id {
			if let Some(pk) = unblinded_pk {
				let payload = ForwardTlvs {
					next_node_id: pk,
					next_blinding_override: None,
				};
				blinded_hops.push(BlindedHop {
					blinded_node_id: prev_blinded_node_id,
					encrypted_payload: encrypt_payload(payload, prev_ss),
				});
			} else { debug_assert!(false); }
		}
		prev_ss_and_blinded_node_id = Some((encrypted_payload_ss, blinded_node_id));
	})?;

	if let Some((final_ss, final_blinded_node_id)) = prev_ss_and_blinded_node_id {
		let final_payload = ReceiveTlvs { path_id: None };
		blinded_hops.push(BlindedHop {
			blinded_node_id: final_blinded_node_id,
			encrypted_payload: encrypt_payload(final_payload, final_ss),
		});
	} else { debug_assert!(false) }

	Ok(blinded_hops)
}

/// Encrypt TLV payload to be used as a [`BlindedHop::encrypted_payload`].
fn encrypt_payload<P: Writeable>(payload: P, encrypted_tlvs_ss: [u8; 32]) -> Vec<u8> {
	let mut writer = VecWriter(Vec::new());
	let write_adapter = ChaChaPolyWriteAdapter::new(encrypted_tlvs_ss, &payload);
	write_adapter.write(&mut writer).expect("In-memory writes cannot fail");
	writer.0
}

impl Writeable for BlindedRoute {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), io::Error> {
		self.introduction_node_id.write(w)?;
		self.blinding_point.write(w)?;
		(self.blinded_hops.len() as u8).write(w)?;
		for hop in &self.blinded_hops {
			hop.write(w)?;
		}
		Ok(())
	}
}

impl Readable for BlindedRoute {
	fn read<R: io::Read>(r: &mut R) -> Result<Self, DecodeError> {
		let introduction_node_id = Readable::read(r)?;
		let blinding_point = Readable::read(r)?;
		let num_hops: u8 = Readable::read(r)?;
		if num_hops == 0 { return Err(DecodeError::InvalidValue) }
		let mut blinded_hops: Vec<BlindedHop> = Vec::with_capacity(num_hops.into());
		for _ in 0..num_hops {
			blinded_hops.push(Readable::read(r)?);
		}
		Ok(BlindedRoute {
			introduction_node_id,
			blinding_point,
			blinded_hops,
		})
	}
}

impl_writeable!(BlindedHop, {
	blinded_node_id,
	encrypted_payload
});

/// TLVs to encode in an intermediate onion message packet's hop data. When provided in a blinded
/// route, they are encoded into [`BlindedHop::encrypted_payload`].
pub(crate) struct ForwardTlvs {
	/// The node id of the next hop in the onion message's path.
	pub(super) next_node_id: PublicKey,
	/// Senders to a blinded route use this value to concatenate the route they find to the
	/// introduction node with the blinded route.
	pub(super) next_blinding_override: Option<PublicKey>,
}

/// Similar to [`ForwardTlvs`], but these TLVs are for the final node.
pub(crate) struct ReceiveTlvs {
	/// If `path_id` is `Some`, it is used to identify the blinded route that this onion message is
	/// sending to. This is useful for receivers to check that said blinded route is being used in
	/// the right context.
	pub(super) path_id: Option<[u8; 32]>,
}

impl Writeable for ForwardTlvs {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		// TODO: write padding
		encode_tlv_stream!(writer, {
			(4, self.next_node_id, required),
			(8, self.next_blinding_override, option)
		});
		Ok(())
	}
}

impl Writeable for ReceiveTlvs {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), io::Error> {
		// TODO: write padding
		encode_tlv_stream!(writer, {
			(6, self.path_id, option),
		});
		Ok(())
	}
}
