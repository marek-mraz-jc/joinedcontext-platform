-- T-4370: a turn whose answer call came back with no text to send, counted apart from a
-- refusal or an error before the call.
ALTER TABLE answer_timings ADD COLUMN no_answer boolean NOT NULL DEFAULT false;
