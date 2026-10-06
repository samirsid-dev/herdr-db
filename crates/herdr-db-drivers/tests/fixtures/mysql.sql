-- Objects that are hard to introspect, MySQL flavour.
DROP DATABASE IF EXISTS herdr_fixture;
CREATE DATABASE herdr_fixture;
USE herdr_fixture;

CREATE TABLE orgs (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(100) NOT NULL UNIQUE COMMENT 'Nom affiché, l''unique',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) COMMENT = 'Organisations clientes';

CREATE TRIGGER orgs_touch BEFORE UPDATE ON orgs FOR EACH ROW SET NEW.created_at = CURRENT_TIMESTAMP;

CREATE TABLE memberships (
    org_id INT NOT NULL,
    user_id BIGINT NOT NULL,
    role ENUM('member', 'admin') NOT NULL DEFAULT 'member',
    PRIMARY KEY (org_id, user_id),
    CONSTRAINT memberships_org_fk FOREIGN KEY (org_id) REFERENCES orgs (id) ON DELETE CASCADE
);

CREATE TABLE `Audit Log` (
    `Id` BIGINT AUTO_INCREMENT PRIMARY KEY,
    org_id INT NOT NULL,
    user_id BIGINT NOT NULL,
    payload JSON,
    price DECIMAL(10, 2) NOT NULL DEFAULT 0,
    qty INT NOT NULL DEFAULT 1,
    total DECIMAL(12, 2) GENERATED ALWAYS AS (price * qty) STORED,
    qty_label VARCHAR(20) GENERATED ALWAYS AS (CONCAT('x', qty)) VIRTUAL,
    note TEXT,
    raw VARBINARY(16),
    CONSTRAINT audit_membership_fk FOREIGN KEY (org_id, user_id) REFERENCES memberships (org_id, user_id),
    CONSTRAINT positive_qty CHECK (qty > 0),
    INDEX audit_note_idx (note(20))
) COMMENT = 'Journal d''audit';

CREATE VIEW org_members AS SELECT o.name, m.user_id FROM orgs o JOIN memberships m ON m.org_id = o.id;

CREATE TABLE digits (d INT PRIMARY KEY);
INSERT INTO digits VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9);

CREATE TABLE big (
    id INT PRIMARY KEY,
    label VARCHAR(20) NOT NULL,
    maybe VARCHAR(5)
);
INSERT INTO big
SELECT n, CONCAT('row ', LPAD(n, 6, '0')), CASE n % 3 WHEN 0 THEN NULL WHEN 1 THEN '' ELSE 'x' END
FROM (
    SELECT a.d + 10 * b.d + 100 * c.d + 1000 * e.d + 10000 * f.d + 100000 * g.d + 1 AS n
    FROM digits a, digits b, digits c, digits e, digits f, digits g
) numbers
WHERE n <= 300000;

INSERT INTO orgs (name) VALUES ('Hellocare'), ('Acme');
INSERT INTO memberships VALUES (1, 10, 'admin'), (1, 11, 'member'), (2, 20, 'member');
INSERT INTO `Audit Log` (org_id, user_id, payload, price, qty, note, raw)
VALUES (1, 10, '{"k": [1, 2]}', 9.5, 2, NULL, X'0102'), (1, 11, NULL, 0, 1, '', NULL);
ANALYZE TABLE big, orgs, memberships, `Audit Log`;
