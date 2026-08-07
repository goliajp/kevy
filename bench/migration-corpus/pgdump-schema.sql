--
-- PostgreSQL database dump
--

\restrict mYJhvf9dpx1v7RtVxkxVPc77atvEnBOs5C96UpMA2uZTOrkB8rS7QCuyB4Ziabv

-- Dumped from database version 18.4 (Debian 18.4-1.pgdg12+1)
-- Dumped by pg_dump version 18.4 (Debian 18.4-1.pgdg12+1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: billing; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.billing (
    id bigint NOT NULL,
    user_id bigint NOT NULL,
    amount money NOT NULL,
    src_ip inet NOT NULL
);


ALTER TABLE public.billing OWNER TO postgres;

--
-- Name: messages; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.messages (
    mid bigint NOT NULL,
    tid bigint NOT NULL,
    author_id bigint NOT NULL,
    sent_at timestamp without time zone NOT NULL,
    body text NOT NULL,
    spam_score bigint NOT NULL
);


ALTER TABLE public.messages OWNER TO postgres;

--
-- Name: threads; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.threads (
    tid bigint NOT NULL,
    owner_id bigint NOT NULL,
    subject text NOT NULL,
    updated_at timestamp without time zone NOT NULL,
    msg_count bigint NOT NULL
);


ALTER TABLE public.threads OWNER TO postgres;

--
-- Name: users; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.users (
    id bigint NOT NULL,
    email text NOT NULL,
    name text NOT NULL,
    created_at timestamp without time zone NOT NULL,
    flags bigint NOT NULL
);


ALTER TABLE public.users OWNER TO postgres;

--
-- Name: billing billing_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.billing
    ADD CONSTRAINT billing_pkey PRIMARY KEY (id);


--
-- Name: messages messages_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.messages
    ADD CONSTRAINT messages_pkey PRIMARY KEY (mid);


--
-- Name: threads threads_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.threads
    ADD CONSTRAINT threads_pkey PRIMARY KEY (tid);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: messages_sent_at_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX messages_sent_at_idx ON public.messages USING btree (sent_at);


--
-- Name: messages_tid_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX messages_tid_idx ON public.messages USING btree (tid);


--
-- Name: threads_owner_id_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX threads_owner_id_idx ON public.threads USING btree (owner_id);


--
-- Name: threads_updated_at_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX threads_updated_at_idx ON public.threads USING btree (updated_at);


--
-- Name: users_email_idx; Type: INDEX; Schema: public; Owner: postgres
--

CREATE UNIQUE INDEX users_email_idx ON public.users USING btree (email);


--
-- PostgreSQL database dump complete
--

\unrestrict mYJhvf9dpx1v7RtVxkxVPc77atvEnBOs5C96UpMA2uZTOrkB8rS7QCuyB4Ziabv

