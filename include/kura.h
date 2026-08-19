/*
 * kura, a storage engine for search and retrieval.
 *
 * This header is the whole public surface of the library. It is written by hand
 * rather than generated, because it is a contract with every host that links the
 * engine and a contract is worth reading. The C smoke test under examples/c is
 * compiled against this file in CI, so a signature that drifts from the Rust
 * side fails the build rather than a caller's process.
 *
 * Conventions:
 *
 *   Every call returns a status code. Results come back through out parameters,
 *   which are written before any failure path returns, so the caller always has
 *   a defined value.
 *
 *   Null is an error, not a crash. Passing a null pointer where one is required
 *   returns KURA_ERR_NULL.
 *
 *   Memory the engine allocates is freed by the engine. A KuraBuffer goes back
 *   to kura_buffer_free and a KuraBitmap goes back to kura_bitmap_free.
 *
 *   The engine is not thread safe per handle. Two threads may use two different
 *   bitmaps at the same time, and must not use the same one.
 */

#ifndef KURA_H
#define KURA_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The version of this ABI. Compare against kura_abi_version() at startup. */
#define KURA_ABI_VERSION 1u

/* Status codes. */
#define KURA_OK 0
#define KURA_ERR_NULL 1
#define KURA_ERR_TRUNCATED 2
#define KURA_ERR_OVERFLOW 3
#define KURA_ERR_BAD_MAGIC 4
#define KURA_ERR_UNSUPPORTED_VERSION 5
#define KURA_ERR_CHECKSUM 6
#define KURA_ERR_DIMENSION_MISMATCH 7
#define KURA_ERR_NOT_SORTED 8
#define KURA_ERR_BUFFER_TOO_SMALL 9
#define KURA_ERR_PANIC 10

/* A block of bytes the engine allocated. Free it with kura_buffer_free. */
typedef struct KuraBuffer {
  uint8_t *data;
  size_t len;
  size_t cap;
} KuraBuffer;

/* An opaque set of document ids. */
typedef struct KuraBitmap KuraBitmap;

/* Library information. */
uint32_t kura_abi_version(void);
const char *kura_version(void);
const char *kura_status_message(int32_t status);

/* Memory owned by the engine. */
void kura_buffer_free(KuraBuffer buffer);

/* Sets of document ids. */
KuraBitmap *kura_bitmap_new(void);
void kura_bitmap_free(KuraBitmap *bitmap);
int32_t kura_bitmap_insert(KuraBitmap *bitmap, uint32_t id);
int32_t kura_bitmap_remove(KuraBitmap *bitmap, uint32_t id);
int32_t kura_bitmap_contains(const KuraBitmap *bitmap, uint32_t id, int32_t *out);
int32_t kura_bitmap_len(const KuraBitmap *bitmap, size_t *out);
int32_t kura_bitmap_intersect(KuraBitmap *bitmap, const KuraBitmap *other);
int32_t kura_bitmap_union(KuraBitmap *bitmap, const KuraBitmap *other);
int32_t kura_bitmap_to_array(const KuraBitmap *bitmap, uint32_t *out, size_t cap, size_t *out_len);

/*
 * Posting lists.
 *
 * kura_postings_encode takes ascending ids and returns compressed bytes.
 * kura_postings_len reads the count out of the header, which is what a caller
 * sizes its buffer from before calling kura_postings_decode.
 * kura_postings_contains answers a membership question by decoding one block,
 * so a host never has to pull a large list across the boundary to ask it.
 */
int32_t kura_postings_encode(const uint32_t *ids, size_t len, KuraBuffer *out);
int32_t kura_postings_len(const uint8_t *data, size_t len, size_t *out);
int32_t kura_postings_decode(const uint8_t *data, size_t len, uint32_t *out, size_t cap, size_t *out_len);
int32_t kura_postings_contains(const uint8_t *data, size_t len, uint32_t id, int32_t *out);

/*
 * Vectors.
 *
 * Quantised vectors are one signed byte per dimension plus a scale. Both parts
 * are needed to reconstruct a vector or to score one, so store them together.
 */
int32_t kura_vector_cosine(const float *a, size_t a_len, const float *b, size_t b_len, float *out);
int32_t kura_vector_quantise(const float *input, size_t len, int8_t *out, float *out_scale);
int32_t kura_vector_dot_quantised(const int8_t *a, float a_scale, const int8_t *b, float b_scale, size_t len,
                                  float *out);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* KURA_H */
