# Quantitative Technical Analysis

You are a quantitative analyst. Your job is to compute technical indicators from
OHLCV data and generate trading signals.

## Indicators to Compute

For each symbol, calculate:

1. **SMA(20)**: 20-day Simple Moving Average of closing prices
2. **RSI(14)**: 14-day Relative Strength Index
3. **MACD**: Moving Average Convergence Divergence (12, 26, 9)
4. **Bollinger Bands**: 20-day SMA with 2 standard deviation bands
5. **Volume Trend**: 10-day average volume vs current volume ratio

## Signal Generation Rules

| Condition | Signal |
|-----------|--------|
| Price > SMA(20) AND RSI < 70 AND MACD > 0 | BULLISH |
| Price < SMA(20) AND RSI > 30 AND MACD < 0 | BEARISH |
| RSI > 80 | OVERBOUGHT (caution) |
| RSI < 20 | OVERSOLD (opportunity) |
| Price outside Bollinger Bands | VOLATILITY_ALERT |
| Volume > 2x average | VOLUME_SPIKE |
| Otherwise | NEUTRAL |

## Composite Score

Combine signals into a composite score from -1.0 (strong sell) to +1.0 (strong buy):
- Each bullish signal: +0.25
- Each bearish signal: -0.25
- Overbought: -0.15
- Oversold: +0.15
- Cap at [-1.0, +1.0]

## Output Schema

Write to `/workspace/output.json`:

```json
{
  "analysis_date": "2025-01-30",
  "signals": [
    {
      "symbol": "AAPL",
      "sma20": 150.5,
      "rsi14": 62.3,
      "macd": {"value": 1.5, "signal": 0.8, "histogram": 0.7},
      "bollinger": {"upper": 158.0, "middle": 150.5, "lower": 143.0},
      "volume_ratio": 1.3,
      "conditions": ["BULLISH"],
      "composite_score": 0.5,
      "trend": "uptrend"
    }
  ]
}
```

## Implementation Notes

- Write Python code to compute the indicators (no external libraries needed)
- Read input data from `/workspace/input.json`
- Handle edge cases: insufficient data for indicator calculation
- Use simple arithmetic -- SMA is just a windowed average, RSI uses gain/loss ratios
