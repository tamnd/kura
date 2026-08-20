/*
 * A minimal C caller, compiled and run in CI against the static library.
 *
 * The point is not coverage, the unit tests do that. The point is that the
 * header, the symbol names and the calling convention are exercised by a real
 * compiler and a real linker on every platform the engine claims to support.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "kura.h"

static int failures = 0;

static void check(int condition, const char *what) {
  if (!condition) {
    fprintf(stderr, "FAIL %s\n", what);
    failures++;
    return;
  }
  printf("ok   %s\n", what);
}

static void check_status(int32_t status, const char *what) {
  if (status != KURA_OK) {
    fprintf(stderr, "FAIL %s: %s\n", what, kura_status_message(status));
    failures++;
    return;
  }
  printf("ok   %s\n", what);
}

static void postings(void) {
  enum { COUNT = 5000 };
  uint32_t ids[COUNT];
  for (uint32_t i = 0; i < COUNT; i++) {
    ids[i] = i * 3;
  }

  KuraBuffer encoded = {NULL, 0, 0};
  check_status(kura_postings_encode(ids, NULL, COUNT, &encoded), "encode a posting list");
  check(encoded.len < sizeof(ids) / 2, "the encoded list is smaller than the raw ids");

  size_t count = 0;
  check_status(kura_postings_len(encoded.data, encoded.len, &count), "read the count");
  check(count == COUNT, "the count survives the round trip");

  uint32_t *decoded = calloc(count, sizeof(uint32_t));
  size_t written = 0;
  check_status(kura_postings_decode(encoded.data, encoded.len, decoded, count, &written), "decode");
  check(written == COUNT && memcmp(decoded, ids, sizeof(ids)) == 0, "every id survives the round trip");
  free(decoded);

  int32_t found = -1;
  check_status(kura_postings_contains(encoded.data, encoded.len, 300, &found), "look up a member");
  check(found == 1, "a member is found");
  check_status(kura_postings_contains(encoded.data, encoded.len, 301, &found), "look up a non member");
  check(found == 0, "a non member is not found");

  kura_buffer_free(encoded);
}

static void bitmaps(void) {
  KuraBitmap *left = kura_bitmap_new();
  KuraBitmap *right = kura_bitmap_new();
  check(left != NULL && right != NULL, "allocate two bitmaps");

  for (uint32_t i = 0; i < 1000; i++) {
    kura_bitmap_insert(left, i);
  }
  for (uint32_t i = 500; i < 1500; i++) {
    kura_bitmap_insert(right, i);
  }
  check_status(kura_bitmap_intersect(left, right), "intersect");

  size_t len = 0;
  check_status(kura_bitmap_len(left, &len), "read the size");
  check(len == 500, "the intersection holds the overlap");

  uint32_t *ids = calloc(len, sizeof(uint32_t));
  size_t written = 0;
  check_status(kura_bitmap_to_array(left, ids, len, &written), "copy the ids out");
  check(written == 500 && ids[0] == 500 && ids[499] == 999, "the ids are the overlap, in order");
  free(ids);

  kura_bitmap_free(left);
  kura_bitmap_free(right);
}

static void vectors(void) {
  const float a[4] = {0.9f, 0.1f, -0.4f, 0.2f};
  const float b[4] = {0.85f, 0.15f, -0.35f, 0.25f};

  float score = 0.0f;
  check_status(kura_vector_cosine(a, 4, b, 4, &score), "score two vectors");
  check(score > 0.9f, "similar vectors score high");

  int32_t status = kura_vector_cosine(a, 4, b, 3, &score);
  check(status == KURA_ERR_DIMENSION_MISMATCH, "a length mismatch is reported");

  int8_t qa[4] = {0};
  int8_t qb[4] = {0};
  float scale_a = 0.0f;
  float scale_b = 0.0f;
  check_status(kura_vector_quantise(a, 4, qa, &scale_a), "quantise the first vector");
  check_status(kura_vector_quantise(b, 4, qb, &scale_b), "quantise the second vector");
  check_status(kura_vector_dot_quantised(qa, scale_a, qb, scale_b, 4, &score), "score the quantised pair");
  check(score > 0.0f, "the quantised pair still scores positive");
}

static void errors(void) {
  size_t count = 0;
  int32_t status = kura_postings_len(NULL, 4, &count);
  check(status == KURA_ERR_NULL, "a null pointer is refused");

  uint8_t garbage[32];
  memset(garbage, 0xff, sizeof(garbage));
  status = kura_postings_len(garbage, sizeof(garbage), &count);
  check(status != KURA_OK, "garbage input is an error, not a crash");

  check(strlen(kura_status_message(status)) > 0, "every status has a message");
}

int main(void) {
  printf("kura %s, abi %u\n", kura_version(), kura_abi_version());
  if (kura_abi_version() != KURA_ABI_VERSION) {
    fprintf(stderr, "FAIL the library and the header disagree about the abi version\n");
    return 1;
  }

  postings();
  bitmaps();
  vectors();
  errors();

  if (failures > 0) {
    fprintf(stderr, "%d checks failed\n", failures);
    return 1;
  }
  printf("all checks passed\n");
  return 0;
}
