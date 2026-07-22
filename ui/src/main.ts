import './app.css';
import App from './App.svelte';
import { mount } from 'svelte';

const root = document.getElementById('app');
if (!root) throw new Error('LocalReview root was not found');

mount(App, { target: root });
