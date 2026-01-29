-- Add host_ip column to pods table for -o wide support
ALTER TABLE pods ADD COLUMN IF NOT EXISTS host_ip VARCHAR(45);
