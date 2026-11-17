package sessionstore

import (
	"context"
	"database/sql"
	"errors"
	"os"
)

// Deleted returns all attempt IDs for a deleted conversation, for idempotent
// resource cleanup after a crash. Tombstones retain identities, not chat text.
func (s *SQLite) Deleted(ctx context.Context, id string) ([]string, error) {
	rows, err := s.db.QueryContext(ctx, `SELECT id FROM deleted_attempts WHERE conversation_id=(SELECT conversation_id FROM deleted_attempts WHERE id=?)`, id)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	ids := []string{}
	for rows.Next() {
		var value string
		if err = rows.Scan(&value); err != nil {
			return nil, err
		}
		ids = append(ids, value)
	}
	return ids, rows.Err()
}

// Delete removes every attempt and the conversation title in one transaction.
// Create rejects tombstoned parents so an in-flight resume cannot resurrect it.
func (s *SQLite) Delete(ctx context.Context, id string) ([]string, error) {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	var conversation string
	err = tx.QueryRowContext(ctx, `SELECT conversation_id FROM attempts WHERE id=?`, id).Scan(&conversation)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, os.ErrNotExist
	}
	if err != nil {
		return nil, err
	}
	rows, err := tx.QueryContext(ctx, `SELECT id FROM attempts WHERE conversation_id=?`, conversation)
	if err != nil {
		return nil, err
	}
	ids := []string{}
	for rows.Next() {
		var value string
		if err = rows.Scan(&value); err != nil {
			rows.Close()
			return nil, err
		}
		ids = append(ids, value)
	}
	err = rows.Err()
	rows.Close()
	if err != nil {
		return nil, err
	}
	if _, err = tx.ExecContext(ctx, `INSERT INTO deleted_attempts(id,conversation_id) SELECT id,conversation_id FROM attempts WHERE conversation_id=?`, conversation); err != nil {
		return nil, err
	}
	if _, err = tx.ExecContext(ctx, `DELETE FROM attempts WHERE conversation_id=?`, conversation); err != nil {
		return nil, err
	}
	if _, err = tx.ExecContext(ctx, `DELETE FROM conversation_metadata WHERE id=?`, conversation); err != nil {
		return nil, err
	}
	return ids, tx.Commit()
}
