USE testdb;
GO

CREATE TABLE dbo.customers (
    customer_id   NVARCHAR(50) NOT NULL PRIMARY KEY,
    email         NVARCHAR(255),
    first_name    NVARCHAR(100),
    last_name     NVARCHAR(100),
    created_at    DATETIME2
);
GO

INSERT INTO dbo.customers (customer_id, email, first_name, last_name, created_at)
VALUES
  ('c1', 'alice@example.com', 'Alice', 'Smith', '2025-01-01T10:00:00'),
  ('c2', 'bob@example.com',   'Bob',   'Jones', '2025-01-02T11:30:00'),
  ('c3', 'carol@example.com', 'Carol', 'Ng',    '2025-01-03T08:05:00'),
  ('c4', 'diego@example.com', 'Diego', 'Rivera','2025-01-04T16:20:00'),
  ('c5', 'eva@example.com',   'Eva',   'Patel', '2025-01-05T12:10:00'),
  ('c6', 'frank@example.com', 'Frank', 'Miller','2025-01-06T09:45:00');
GO

CREATE TABLE dbo.orders (
    order_id      NVARCHAR(50) NOT NULL PRIMARY KEY,
    customer_id   NVARCHAR(50),
    order_status  NVARCHAR(50),
    total_amount  DECIMAL(10,2),
    placed_at     DATETIME2
);
GO

INSERT INTO dbo.orders (order_id, customer_id, order_status, total_amount, placed_at)
VALUES
  ('o1', 'c1', 'PAID', 120.50, '2025-01-03T09:15:00'),
  ('o2', 'c2', 'PAID',  75.00, '2025-01-04T14:45:00'),
  ('o3', 'c1', 'PAID',  42.25, '2025-01-05T10:05:00'),
  ('o4', 'c3', 'REFUNDED', 18.00, '2025-01-06T12:00:00'),
  ('o5', 'c4', 'PAID', 250.00, '2025-01-07T17:30:00'),
  ('o6', 'c5', 'PENDING', 33.33, '2025-01-08T08:40:00'),
  ('o7', 'c6', 'PAID', 501.75, '2025-01-09T19:10:00'),
  ('o8', 'c2', 'CANCELLED', 12.99, '2025-01-10T07:25:00');
GO

CREATE TABLE dbo.order_items (
    order_item_id NVARCHAR(50) NOT NULL PRIMARY KEY,
    order_id      NVARCHAR(50),
    product_sku   NVARCHAR(100),
    quantity      INT,
    unit_price    DECIMAL(10,2)
);
GO

INSERT INTO dbo.order_items (order_item_id, order_id, product_sku, quantity, unit_price)
VALUES
  ('oi1', 'o1', 'SKU-RED',   2, 50.00),
  ('oi2', 'o1', 'SKU-BLUE',  1, 20.50),
  ('oi3', 'o2', 'SKU-GREEN', 1, 75.00),
  ('oi4', 'o3', 'SKU-BLUE',  1, 20.50),
  ('oi5', 'o3', 'SKU-YELLOW',1, 21.75),
  ('oi6', 'o4', 'SKU-RED',   1, 18.00),
  ('oi7', 'o5', 'SKU-BLACK', 5, 50.00),
  ('oi8', 'o6', 'SKU-WHITE', 3, 11.11),
  ('oi9', 'o7', 'SKU-GOLD',  2, 199.99),
  ('oi10','o7', 'SKU-SILVER',1, 101.77),
  ('oi11','o8', 'SKU-GREEN', 1, 12.99);
GO
