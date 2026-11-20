-- Tables of the MI_P merged index (Part > Partsupp > Lineitem), and the
-- predicate-free maintained input to TPC-H Q14's residual computation.
--
-- Each table holds its primary key, the columns MI_P's key trie uses, and the
-- columns its payloads carry. The payloads serve Q14 for now; adding a query
-- adds its columns here and in mi_p.mi, then regenerates src/generated.rs.

CREATE TABLE Part (
    p_partkey BIGINT NOT NULL PRIMARY KEY,
    p_type VARCHAR NOT NULL
);

CREATE TABLE Partsupp (
    ps_partkey BIGINT NOT NULL,
    ps_suppkey BIGINT NOT NULL,
    ps_availqty INTEGER NOT NULL,
    PRIMARY KEY (ps_partkey, ps_suppkey)
);

CREATE TABLE Lineitem (
    l_orderkey BIGINT NOT NULL,
    l_linenumber INTEGER NOT NULL,
    l_partkey BIGINT NOT NULL,
    l_suppkey BIGINT NOT NULL,
    l_extendedprice DECIMAL(15, 2) NOT NULL,
    l_discount DECIMAL(4, 2) NOT NULL,
    l_shipdate DATE NOT NULL,
    PRIMARY KEY (l_orderkey, l_linenumber)
);

CREATE MATERIALIZED VIEW V AS
SELECT p.p_partkey, p.p_type,
       l.l_orderkey, l.l_linenumber, l.l_extendedprice, l.l_discount, l.l_shipdate
FROM Part AS p
JOIN Lineitem AS l ON p.p_partkey = l.l_partkey;
