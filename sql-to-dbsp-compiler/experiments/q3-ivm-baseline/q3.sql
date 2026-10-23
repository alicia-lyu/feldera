-- View circuit: the same tables without keys, so they compile to Z-set inputs, and the
-- predicate-free maintained input to TPC-H Q3's residual computation. The driver feeds each
-- table the base circuit's net delta (base.sql), with full rows for deletions.
CREATE TABLE Customer (
    c_custkey BIGINT NOT NULL,
    c_mktsegment VARCHAR NOT NULL
);

CREATE TABLE Orders (
    o_orderkey BIGINT NOT NULL,
    o_custkey BIGINT NOT NULL,
    o_orderdate DATE NOT NULL,
    o_shippriority INTEGER NOT NULL
);

CREATE TABLE Lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INTEGER NOT NULL,
    l_extendedprice DECIMAL(15, 2) NOT NULL,
    l_discount DECIMAL(4, 2) NOT NULL,
    l_shipdate DATE NOT NULL
);

CREATE MATERIALIZED VIEW V AS
SELECT c.c_custkey, c.c_mktsegment,
       o.o_orderkey, o.o_orderdate, o.o_shippriority,
       l.l_linenumber, l.l_extendedprice, l.l_discount, l.l_shipdate
FROM Customer AS c
JOIN Orders AS o ON c.c_custkey = o.o_custkey
JOIN Lineitem AS l ON o.o_orderkey = l.l_orderkey;
