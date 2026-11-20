-- Native view circuit: the tables V reads, without keys, so they compile to Z-set inputs, and
-- V, the predicate-free maintained input to TPC-H Q14's residual computation. V is
-- ../merged-index-p/schema.sql's. The driver feeds each table the base circuit's net delta
-- (base.sql), with full rows for deletions. V reads no Partsupp, so this circuit has none.
CREATE TABLE Part (
    p_partkey BIGINT NOT NULL,
    p_type VARCHAR NOT NULL
);

CREATE TABLE Lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INTEGER NOT NULL,
    l_partkey BIGINT NOT NULL,
    l_suppkey BIGINT NOT NULL,
    l_extendedprice DECIMAL(15, 2) NOT NULL,
    l_discount DECIMAL(4, 2) NOT NULL,
    l_shipdate DATE NOT NULL
);

CREATE MATERIALIZED VIEW V AS
SELECT p.p_partkey, p.p_type,
       l.l_orderkey, l.l_linenumber, l.l_extendedprice, l.l_discount, l.l_shipdate
FROM Part AS p
JOIN Lineitem AS l ON p.p_partkey = l.l_partkey;
