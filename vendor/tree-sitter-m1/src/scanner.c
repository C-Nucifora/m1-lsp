/*
 * scanner.c — external scanner for the M1 tree-sitter grammar.
 *
 * Produces a single external token: `identifier`. An identifier is one path
 * *segment*: a run of words ([A-Za-z_][A-Za-z0-9_]*) joined by single spaces,
 * where no word is a reserved keyword.
 *
 * The tricky case this exists to handle:
 *
 *     Pellow.KVB Bonquil eq Wexlar Bonquil Mosko.Vor
 *             ^^^^^^^^^^                              segment "KVB Bonquil"
 *                        ^^                           keyword `eq` (NOT absorbed)
 *
 * (The example name above is a synthetic placeholder, not from any real project.)
 *
 * We greedily join "word SPACE word" but stop before a word that is reserved,
 * using mark_end() so the trailing " eq" is returned to the lexer as
 * look-ahead rather than being swallowed into the identifier.
 *
 * v2: a standalone `$(...)` operand (e.g. `x = $(SEG) + 1`) is emitted as a
 * separate INTERPOLATION token; a `$(...)` that leads a multi-word name folds
 * into the IDENTIFIER segment as before.
 */

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <string.h>

enum TokenType { IDENTIFIER, INTERPOLATION };

/* Mirrors the keyword tokens in grammar.js. Keep the two in sync. */
static const char *const RESERVED[] = {
    "local", "if", "else", "and", "or", "not", "eq", "neq", "true", "false",
    "static", "when", "is", "expand", "to",
};
static const unsigned RESERVED_COUNT = sizeof(RESERVED) / sizeof(RESERVED[0]);

static bool is_word_start(int32_t c) {
  return c == '_' || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
}

static bool is_word_char(int32_t c) {
  return is_word_start(c) || (c >= '0' && c <= '9');
}

/* Read a maximal word into buf (NUL-terminated, truncated to cap). */
static void read_word(TSLexer *lexer, char *buf, unsigned cap) {
  unsigned n = 0;
  while (is_word_char(lexer->lookahead)) {
    if (n + 1 < cap) {
      buf[n] = (char)lexer->lookahead;
    }
    n++;
    lexer->advance(lexer, false);
  }
  buf[n < cap ? n : cap - 1] = '\0';
}

static bool is_reserved(const char *word) {
  for (unsigned i = 0; i < RESERVED_COUNT; i++) {
    if (strcmp(word, RESERVED[i]) == 0) {
      return true;
    }
  }
  return false;
}

/* Consume a `$(...)` compile-time interpolation, e.g. `$(SEG)`. These appear
 * both as standalone operands (`( $(SEG) - 1)`) and as units inside a name
 * segment (`naxID Bnk $(SEG) Vlim $(NODE)`). Returns true if a complete
 * interpolation was consumed. */
static bool try_read_interp(TSLexer *lexer) {
  if (lexer->lookahead != '$') {
    return false;
  }
  lexer->advance(lexer, false);
  if (lexer->lookahead != '(') {
    return false;
  }
  lexer->advance(lexer, false);
  while (lexer->lookahead != ')' && lexer->lookahead != 0) {
    lexer->advance(lexer, false);
  }
  if (lexer->lookahead != ')') {
    return false;
  }
  lexer->advance(lexer, false);
  return true;
}

void *tree_sitter_m1_external_scanner_create(void) { return NULL; }
void tree_sitter_m1_external_scanner_destroy(void *payload) { (void)payload; }
void tree_sitter_m1_external_scanner_reset(void *payload) { (void)payload; }

unsigned tree_sitter_m1_external_scanner_serialize(void *payload, char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_m1_external_scanner_deserialize(void *payload, const char *buffer,
                                                 unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

bool tree_sitter_m1_external_scanner_scan(void *payload, TSLexer *lexer,
                                          const bool *valid_symbols) {
  (void)payload;
  if (!valid_symbols[IDENTIFIER] && !valid_symbols[INTERPOLATION]) {
    return false;
  }

  /* Skip leading whitespace; it is never part of the token. */
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\n' || lexer->lookahead == '\r') {
    lexer->advance(lexer, true);
  }

  bool leads_with_interp = false;

  /* First unit: a `$(...)` interpolation or a word. */
  if (lexer->lookahead == '$') {
    if (!try_read_interp(lexer)) {
      return false;
    }
    leads_with_interp = true;
  } else if (is_word_start(lexer->lookahead)) {
    char word[64];
    read_word(lexer, word, sizeof(word));
    if (is_reserved(word)) {
      return false; /* let the grammar match the keyword */
    }
  } else {
    return false;
  }
  lexer->mark_end(lexer); /* token currently ends after the first unit */

  /* A standalone `$(...)` operand becomes INTERPOLATION unless it is the head of
   * a multi-word name. "Head of a name" means a space is followed by a genuine
   * continuation unit (a word or another `$(...)`). A space followed by an
   * operator/`;`/anything else (e.g. `$(SEG) + 1`) is still a standalone
   * operand. We only know which after attempting the continuation loop, so we
   * track whether it committed any unit. */
  if (leads_with_interp && !valid_symbols[IDENTIFIER]) {
    /* Identifier not allowed here: only an interpolation can stand. */
    if (!valid_symbols[INTERPOLATION]) {
      return false;
    }
    lexer->result_symbol = INTERPOLATION;
    return true;
  }
  if (!leads_with_interp && !valid_symbols[IDENTIFIER]) {
    return false;
  }

  /* Extend with " <unit>" while the next unit is not a reserved word. A
   * continuation word may begin with a digit ("XV Glim 4", "Glonk 9"); a unit
   * may also be a `$(...)` interpolation ("naxID Bnk $(SEG) Vlim $(NODE)"). */
  bool extended = false;
  for (;;) {
    if (lexer->lookahead != ' ') {
      break;
    }
    lexer->advance(lexer, false); /* tentatively consume the space */
    if (lexer->lookahead == '$') {
      if (!try_read_interp(lexer)) {
        break;
      }
    } else if (is_word_char(lexer->lookahead)) {
      char next[64];
      read_word(lexer, next, sizeof(next));
      if (is_reserved(next)) {
        break; /* leave the consumed " <kw>" as look-ahead via mark_end */
      }
    } else {
      break;
    }
    lexer->mark_end(lexer); /* commit this unit into the identifier */
    extended = true;
  }

  /* A leading `$(...)` that gained no continuation unit is a standalone operand:
   * emit INTERPOLATION (when valid) rather than a single-segment identifier. */
  if (leads_with_interp && !extended && valid_symbols[INTERPOLATION]) {
    lexer->result_symbol = INTERPOLATION;
    return true;
  }

  lexer->result_symbol = IDENTIFIER;
  return true;
}
