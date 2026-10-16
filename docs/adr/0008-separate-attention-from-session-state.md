# Separate attention reasons from Session lifecycle state

SylvOps records a bounded Attention Reason independently of Session lifecycle state because the same state can require different user responses, while recoverable and terminal failures must not be conflated. The daemon owns and persists the reason; clients use it to present consistent grouping, styling, and notifications without redefining lifecycle semantics.
