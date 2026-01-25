import csv
import math
import random
import string
from datetime import date, timedelta

# Paths
ROOT = "barter/examples/data"
MARKS_PATH = f"{ROOT}/bond_marks_template.csv"
SIGNALS_PATH = f"{ROOT}/bond_signals_template.csv"

start_date = date(2025, 1, 1)
end_date = date(2026, 1, 1)

# ---------------------------------------------------------
# Helper Functions
# ---------------------------------------------------------
def generate_security_id():
    """Generate a random 9-digit string security ID."""
    return ''.join(random.choices(string.digits, k=9))

def generate_trade_suffix():
    """Generate a random 4-digit string for trade ID."""
    return ''.join(random.choices(string.digits, k=4))

# ---------------------------------------------------------
# 1. Setup Universe
# ---------------------------------------------------------
NUM_SECURITIES = 500
security_ids = [generate_security_id() for _ in range(NUM_SECURITIES)]
# Ensure uniqueness (highly likely with 9 digits, but good practice)
security_ids = list(set(security_ids)) 
while len(security_ids) < NUM_SECURITIES:
    security_ids.append(generate_security_id())
    security_ids = list(set(security_ids))

# Base price parameters for each security to give them distinct behaviors
# Each security has a base price level and a volatility multiplier
security_params = {}
for sec_id in security_ids:
    security_params[sec_id] = {
        'base_price': 100.0 + random.uniform(-10, 10), # Start around 90-110
        'drift_speed': random.uniform(0.5, 2.0),
        'volatility': random.uniform(0.01, 0.05),
        'phase_shift': random.uniform(0, 2 * math.pi)
    }

# ---------------------------------------------------------
# 2. Generate MARKS (Daily, Clean Price)
# ---------------------------------------------------------
rows_marks = []
marks_by_date_sec = {}
cur = start_date
t = 0

# To track valid dates for signal generation
valid_dates = []

print(f"Generating marks for {NUM_SECURITIES} securities...")

while cur <= end_date:
    valid_dates.append(cur)
    
    # Global market factor (e.g., interest rate moves affecting all bonds)
    market_factor = 2.0 * math.sin(t / 120.0) + (t / 365.0)
    
    for sec_id in security_ids:
        params = security_params[sec_id]
        
        # Individual bond dynamics
        # Price = Base + MarketFactor + IdiosyncraticDrift + Noise
        
        # Slow idiosyncratic drift
        idio_drift = params['drift_speed'] * math.sin(t / 60.0 + params['phase_shift'])
        
        # Random noise
        noise = random.gauss(0, params['volatility'])
        
        price = params['base_price'] + market_factor + idio_drift + noise
        
        # Clamp to realistic bond prices (distressed to premium)
        price = max(40.0, min(150.0, price))
        
        rows_marks.append((cur, sec_id, "CLEAN_PRICE", f"{price:.4f}"))
        marks_by_date_sec[(cur, sec_id)] = price
        
    cur += timedelta(days=1)
    t += 1

print(f"Generated {len(rows_marks)} mark rows.")

with open(MARKS_PATH, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["ts", "security_id", "mark_type", "mark"])
    for r in rows_marks:
        w.writerow([r[0].isoformat(), r[1], r[2], r[3]])

# ---------------------------------------------------------
# 3. Generate SIGNALS (Random Sparse Trades)
# ---------------------------------------------------------
# We will generate random PAIR trades and SINGLE name trades
# Pair Trade: Long A, Short B
# Trade ID Format: YYYYMMDD + SecID1[_SecID2...] + RND4

rows_signals = []
NUM_TRADES = 50 

print(f"Generating {NUM_TRADES} sparse trades...")

for _ in range(NUM_TRADES):
    # Pick a random entry date (leaving at least 20 days for holding)
    entry_idx = random.randint(0, len(valid_dates) - 21)
    entry_date = valid_dates[entry_idx]
    
    # Holding period 5 to 20 days
    holding_days = random.randint(5, 20)
    exit_date = valid_dates[entry_idx + holding_days]
    
    # Randomly decide if Pair trade (80%) or Single trade (20%)
    is_pair = random.random() < 0.8
    
    if is_pair:
        # Pick two distinct securities
        legs = random.sample(security_ids, 2)
        sec_a, sec_b = legs[0], legs[1]
        
        # Trade ID construction
        # cleanup date string for ID
        date_str = entry_date.strftime("%Y%m%d")
        rnd_suffix = generate_trade_suffix()
        trade_id = f"{date_str}_{sec_a}_{sec_b}_{rnd_suffix}"
        
        # Strategy: Long A, Short B (Mean Reversion bet)
        # Weights: +/- 5% (0.05)
        
        # ENTRY
        entry_mark_a = marks_by_date_sec[(entry_date, sec_a)]
        issue_concession_a = random.uniform(0.001, 0.005)
        issue_price_a = entry_mark_a * (1.0 - issue_concession_a)
        rows_signals.append([
            entry_date.isoformat(),
            trade_id,
            sec_a,
            "0.05",
            f"{issue_price_a:.4f}",
        ])
        rows_signals.append([entry_date.isoformat(), trade_id, sec_b, "-0.05", ""])
        
        # EXIT (Reverse signs)
        rows_signals.append([exit_date.isoformat(), trade_id, sec_a, "-0.05", ""])
        rows_signals.append([exit_date.isoformat(), trade_id, sec_b, "0.05", ""])
        
    else:
        # Single name trade (e.g. directional bet)
        sec = random.choice(security_ids)
        
        date_str = entry_date.strftime("%Y%m%d")
        rnd_suffix = generate_trade_suffix()
        trade_id = f"{date_str}_{sec}_{rnd_suffix}"
        
        # Direction: Randomly Long or Short
        direction = 1 if random.random() > 0.5 else -1
        weight = 0.05 * direction
        
        # ENTRY
        if weight > 0:
            entry_mark = marks_by_date_sec[(entry_date, sec)]
            issue_concession = random.uniform(0.001, 0.005)
            issue_price = entry_mark * (1.0 - issue_concession)
            fill_override = f"{issue_price:.4f}"
        else:
            fill_override = ""
        rows_signals.append([entry_date.isoformat(), trade_id, sec, f"{weight:.4f}", fill_override])
        
        # EXIT
        rows_signals.append([exit_date.isoformat(), trade_id, sec, f"{-weight:.4f}", ""])

# Sort signals by date for cleaner CSV (optional but nice)
rows_signals.sort(key=lambda x: x[0])

print(f"Generated {len(rows_signals)} signal rows.")

with open(SIGNALS_PATH, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["ts", "trade_id", "security_id", "delta_weight", "fill_mark_override"])
    w.writerows(rows_signals)
