//! Proof of knowledge for the messages.

use alloc::vec::Vec;

use ark_ec::pairing::Pairing;

use ark_serialize::{CanonicalSerialize, Write};
use ark_std::{cfg_iter, rand::RngCore};

use dock_crypto_utils::join;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use schnorr_pok::{error::SchnorrError, SchnorrChallengeContributor};

use super::UnpackedBlindedMessages;
use crate::{
    helpers::{schnorr_error, DoubleEndedExactSizeIterator, WithSchnorrAndBlindings},
    pairs,
    setup::SignatureParams,
    signature::message_commitment::MessageCommitmentRandomness,
    CommitMessage,
};

pub mod error;
pub mod multi_message_commitment;
mod proof;
mod witnesses;

use crate::signature::MessageCommitment;
pub use error::*;
pub use multi_message_commitment::MultiMessageCommitment;
use multi_message_commitment::*;
pub use proof::*;
use witnesses::*;

/// Generates proof of knowledge for the supplied messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagesPoKGenerator<'a, E: Pairing> {
    /// `com = g * o + \sum_{i}(h_{i} * m_{i})`
    com: WithSchnorrAndBlindings<E::G1Affine, MultiMessageCommitment<E>>,
    /// `com_{j} = g * o_{j} + h * m_{j}`
    com_j: Vec<WithSchnorrAndBlindings<E::G1Affine, MessageCommitment<E>>>,
    witnesses: MessagesPoKWitnesses<'a, E>,
}

type Result<T, E = MessagesPoKError> = core::result::Result<T, E>;

impl<'a, E: Pairing> MessagesPoKGenerator<'a, E> {
    /// Initializes Commitments Proof of Knowledge generator with supplied params.
    /// Each message can be either randomly blinded, unblinded, or blinded using supplied blinding.
    /// By default, a message is blinded with random blinding.
    pub fn init<CMI, R: RngCore>(
        rng: &mut R,
        messages_to_commit: CMI,
        params: &SignatureParams<E>,
        h: &E::G1Affine,
    ) -> Result<Self>
    where
        CMI: IntoIterator,
        CMI::Item: Into<CommitMessage<'a, E::ScalarField>>,
    {
        let UnpackedBlindedMessages(h_arr, messages, blindings) =
            UnpackedBlindedMessages::new(rng, messages_to_commit, &params.h)?;

        // Capture `m` and generates random `o` along with a vector of `o` paired with `m`
        let witnesses = MessagesPoKWitnesses::new(rng, messages);

        let h_blinging_pairs = pairs!(h_arr, blindings);
        // Create new randomness `o` and capture `blindings`, `g`, and `h` from signature params.
        let com_randomness =
            MultiMessageCommitmentRandomness::<E>::init(rng, h_blinging_pairs, &params.g);
        // Create new randomness `o` and captures `blindings` along with `h`, and `g` from signature params.
        let com_j_randomness = MessageCommitmentRandomness::init(rng, &blindings, h, params);

        let MessagesPoKWitnesses { o, o_m_pairs } = &witnesses;
        let (o_arr, m) = o_m_pairs.as_ref().split();

        let h_m_pairs = pairs!(h_arr, m);
        let o_m_iter = cfg_iter!(o_arr).zip(cfg_iter!(m).copied());

        let (com, com_schnorr, com_j) = join!(
            MultiMessageCommitment::new(h_m_pairs, params, o),
            com_randomness.commit(),
            MessageCommitment::new_iter(o_m_iter, h, params)
                .zip(com_j_randomness.commit())
                .map(Into::into)
                .collect()
        );

        Ok(Self {
            com: (com, com_schnorr).into(),
            com_j,
            witnesses,
        })
    }

    /// The commitment's contribution to the overall challenge of the protocol.
    pub fn challenge_contribution<W: Write>(
        &self,
        mut writer: W,
        &SignatureParams {
            g, h: ref h_arr, ..
        }: &SignatureParams<E>,
        h: &E::G1Affine,
    ) -> Result<(), SchnorrError> {
        // `com = g * o + \sum_{i}(h_{i} * m_{i})`
        g.serialize_compressed(&mut writer)?;
        h_arr.serialize_compressed(&mut writer)?;
        self.com.challenge_contribution(&mut writer)?;

        // `com_{j} = g * o_{j} + h * m_{j}`
        h.serialize_compressed(&mut writer)?;
        for com_j in &self.com_j {
            com_j.challenge_contribution(&mut writer)?;
        }

        Ok(())
    }

    /// Generate proof. Post-challenge phase of the protocol.
    pub fn gen_proof(&self, challenge: &E::ScalarField) -> Result<MessagesPoK<E>> {
        let Self {
            witnesses: MessagesPoKWitnesses { o, o_m_pairs },
            ..
        } = self;

        let (com_resp, com_j_resp) = join!(
            {
                // Schnorr response for relation `com = g * o + \sum_{i}(h_{i} * m_{i})`
                let m = o_m_pairs.as_ref().right();

                self.com
                    .response(o, m.iter().copied(), challenge)
                    .map_err(schnorr_error)
                    .map_err(MessagesPoKError::ComProofGenerationFailed)
            },
            {
                // Schnorr responses for relation `com_{j} = g * o_{j} + h * m_{j}`
                if self.com_j.len() != o_m_pairs.len() {
                    Err(MessagesPoKError::IncompatibleComJAndMessages {
                        com_j_len: self.com_j.len(),
                        messages_len: o_m_pairs.len(),
                    })?
                }

                cfg_iter!(self.com_j)
                    .zip(o_m_pairs.as_ref())
                    .enumerate()
                    .map(|(index, (com_j, (o, m)))| {
                        com_j
                            .response(o, m, challenge)
                            .map_err(schnorr_error)
                            .map_err(|error| MessagesPoKError::ComJProofGenerationFailed {
                                index,
                                error,
                            })
                    })
                    .collect::<Result<_>>()
            }
        );

        Ok(MessagesPoK {
            com_resp: com_resp?,
            com_j_resp: com_j_resp?,
        })
    }

    /// Returns underlying blindings.
    pub fn blindings(
        &self,
    ) -> impl DoubleEndedExactSizeIterator<Item = &E::ScalarField> + Clone + '_ {
        self.witnesses.o_m_pairs.as_ref().left().iter()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::iter::empty;

    use ark_bls12_381::Bls12_381;
    use ark_ec::{pairing::Pairing, CurveGroup};
    use ark_std::{
        rand::{rngs::StdRng, SeedableRng},
        One,
    };
    use blake2::Blake2b512;
    use schnorr_pok::compute_random_oracle_challenge;

    use crate::{
        helpers::{rand, IndexIsOutOfBounds},
        setup::test_setup,
        CommitMessage, MessageUnpackingError, MessagesPoKError,
    };
    use ark_std::UniformRand;

    use super::MessagesPoKGenerator;

    type Fr = <Bls12_381 as Pairing>::ScalarField;
    type G1 = <Bls12_381 as Pairing>::G1;

    #[test]
    fn basic() {
        for message_count in 1..=20 {
            let mut rng = StdRng::seed_from_u64(0u64);
            let (_, _, params, messages) =
                test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

            let h = G1::rand(&mut rng).into_affine();

            let pok = MessagesPoKGenerator::init(&mut rng, &messages, &params, &h).unwrap();

            let mut chal_bytes_prover = vec![];
            pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
                .unwrap();
            let challenge_prover =
                compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

            let proof = pok.clone().gen_proof(&challenge_prover).unwrap();

            proof
                .verify(&challenge_prover, empty(), &params, &h)
                .unwrap();
        }
    }

    #[test]
    fn some_messages_unblinded() {
        for message_count in 2..=20 {
            let mut rng = StdRng::seed_from_u64(0u64);
            let (_, _, params, messages) =
                test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

            let h = G1::rand(&mut rng).into_affine();

            let pok = MessagesPoKGenerator::init(
                &mut rng,
                messages.iter().enumerate().map(|(idx, msg)| {
                    if idx % 2 == 0 {
                        CommitMessage::RevealMessage
                    } else {
                        CommitMessage::BlindMessageRandomly(msg)
                    }
                }),
                &params,
                &h,
            )
            .unwrap();

            assert_eq!(messages.len() / 2, pok.com_j.len());

            let mut chal_bytes_prover = vec![];
            pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
                .unwrap();
            let challenge_prover =
                compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

            let proof = pok.clone().gen_proof(&challenge_prover).unwrap();

            proof
                .verify(
                    &challenge_prover,
                    (0..messages.len()).step_by(2),
                    &params,
                    &h,
                )
                .unwrap();
        }
    }

    #[test]
    fn message_count_exceeded() {
        let message_count = 10;
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_, _, params, messages) =
            test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

        let h = G1::rand(&mut rng).into_affine();

        assert_eq!(
            MessagesPoKGenerator::init(
                &mut rng,
                messages
                    .iter()
                    .cycle()
                    .enumerate()
                    .map(|(idx, msg)| if idx % 2 == 0 {
                        CommitMessage::BlindMessageRandomly(msg)
                    } else {
                        CommitMessage::RevealMessage
                    })
                    .take(100),
                &params,
                &h
            )
            .unwrap_err(),
            MessagesPoKError::MessageInputError(MessageUnpackingError::MessageIndexIsOutOfBounds(
                IndexIsOutOfBounds {
                    index: 10,
                    length: message_count
                }
            ))
        );
    }

    #[test]
    fn message_count_less_than_expected() {
        let message_count = 10;
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_, _, params, messages) =
            test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

        let h = G1::rand(&mut rng).into_affine();

        assert_eq!(
            MessagesPoKGenerator::init(
                &mut rng,
                messages
                    .iter()
                    .cycle()
                    .enumerate()
                    .map(|(idx, msg)| if idx % 2 == 0 {
                        CommitMessage::BlindMessageRandomly(msg)
                    } else {
                        CommitMessage::RevealMessage
                    })
                    .take(9),
                &params,
                &h
            )
            .unwrap_err(),
            MessagesPoKError::MessageInputError(MessageUnpackingError::LessMessagesThanExpected {
                provided: 9,
                expected: 10
            })
        );
    }

    #[test]
    fn invalid_revealed_indices_order() {
        for message_count in 4..=20 {
            let mut rng = StdRng::seed_from_u64(0u64);
            let (_, _, params, messages) =
                test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

            let h = G1::rand(&mut rng).into_affine();

            let pok = MessagesPoKGenerator::init(
                &mut rng,
                messages.iter().enumerate().map(|(idx, msg)| {
                    if idx % 2 == 0 {
                        CommitMessage::RevealMessage
                    } else {
                        CommitMessage::BlindMessageRandomly(msg)
                    }
                }),
                &params,
                &h,
            )
            .unwrap();

            assert_eq!(messages.len() / 2, pok.com_j.len());

            let mut chal_bytes_prover = vec![];
            pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
                .unwrap();
            let challenge_prover =
                compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

            let proof = pok.clone().gen_proof(&challenge_prover).unwrap();
            let mut indices = (0..messages.len()).step_by(2).rev();

            assert_eq!(
                proof.verify(&challenge_prover, indices.clone(), &params, &h,),
                Err(MessagesPoKError::RevealedIndicesMustBeUniqueAndSorted {
                    previous: indices.next().unwrap(),
                    current: indices.next().unwrap()
                })
            );
        }
    }

    #[test]
    fn custom_blindings() {
        for message_count in 1..=20 {
            let mut rng = StdRng::seed_from_u64(0u64);
            let (_, _, params, messages) =
                test_setup::<Bls12_381, Blake2b512, _>(&mut rng, message_count);

            let h = G1::rand(&mut rng).into_affine();

            let pok = MessagesPoKGenerator::init(
                &mut rng,
                messages
                    .iter()
                    .map(|message| CommitMessage::BlindMessageWithConcreteBlinding {
                        message,
                        blinding: Fr::one(),
                    }),
                &params,
                &h,
            )
            .unwrap();

            let mut chal_bytes_prover = vec![];
            pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
                .unwrap();
            let challenge_prover =
                compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

            let proof = pok.clone().gen_proof(&challenge_prover).unwrap();

            proof
                .verify(&challenge_prover, empty(), &params, &h)
                .unwrap();
        }
    }

    #[test]
    fn invalid_response() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_, _, params, messages) = test_setup::<Bls12_381, Blake2b512, _>(&mut rng, 1);

        let h = G1::rand(&mut rng).into_affine();

        let pok = MessagesPoKGenerator::init(
            &mut rng,
            messages
                .iter()
                .map(|message| CommitMessage::BlindMessageWithConcreteBlinding {
                    message,
                    blinding: Fr::one(),
                }),
            &params,
            &h,
        )
        .unwrap();

        let mut chal_bytes_prover = vec![];
        pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
            .unwrap();
        let challenge_prover =
            compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

        let mut proof = pok.clone().gen_proof(&challenge_prover).unwrap();
        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_ok());

        proof.com_resp.response.0[0] = rand(&mut rng);

        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_err());
    }

    #[test]
    fn invalid_com_response() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_, _, params, messages) = test_setup::<Bls12_381, Blake2b512, _>(&mut rng, 1);

        let h = G1::rand(&mut rng).into_affine();

        let pok = MessagesPoKGenerator::init(
            &mut rng,
            messages
                .iter()
                .map(|message| CommitMessage::BlindMessageWithConcreteBlinding {
                    message,
                    blinding: Fr::one(),
                }),
            &params,
            &h,
        )
        .unwrap();

        let mut chal_bytes_prover = vec![];
        pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
            .unwrap();
        let challenge_prover =
            compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

        let mut proof = pok.clone().gen_proof(&challenge_prover).unwrap();
        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_ok());

        *proof.com_resp.value = G1::rand(&mut rng).into_affine();

        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_err());
    }

    #[test]
    fn invalid_com_j_response() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_, _, params, messages) = test_setup::<Bls12_381, Blake2b512, _>(&mut rng, 1);

        let h = G1::rand(&mut rng).into_affine();

        let pok = MessagesPoKGenerator::init(
            &mut rng,
            messages
                .iter()
                .map(|message| CommitMessage::BlindMessageWithConcreteBlinding {
                    message,
                    blinding: Fr::one(),
                }),
            &params,
            &h,
        )
        .unwrap();

        let mut chal_bytes_prover = vec![];
        pok.challenge_contribution(&mut chal_bytes_prover, &params, &h)
            .unwrap();
        let challenge_prover =
            compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

        let mut proof = pok.clone().gen_proof(&challenge_prover).unwrap();

        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_ok());

        *proof.com_j_resp.first_mut().unwrap().value = G1::rand(&mut rng).into_affine();

        assert!(proof
            .verify(&challenge_prover, empty(), &params, &h)
            .is_err());
    }

    #[test]
    fn empty_proof() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let (_sk, _pk, params, _messages) = test_setup::<Bls12_381, Blake2b512, _>(&mut rng, 1);
        let h = G1::rand(&mut rng).into_affine();

        assert_eq!(
            MessagesPoKGenerator::init(&mut rng, &[], &params, &h),
            Err(MessagesPoKError::MessageInputError(
                MessageUnpackingError::NoMessagesProvided
            ))
        );
    }
}
